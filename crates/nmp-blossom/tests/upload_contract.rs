//! #545 falsifiers for the BUD-02 upload contract: authorization binding,
//! descriptor integrity, build-time expiration refusal, pre-socket local
//! host admission, the separated failure taxonomy, and response bounding.
//! Each test doc-comment names the invariant it would falsify.

use std::collections::BTreeSet;
use std::net::TcpListener;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use nostr::{Alphabet, Event, EventBuilder, JsonUtil, Keys, Kind, Tag, TagKind, Tags, Timestamp};

use nmp_blossom::{
    upload_authorization_draft, AuthDraftError, AuthValidationError, BlossomClient,
    BlossomClientConfig, BlossomServerUrl, BlossomVerb, ExpectedAuthorization, Sha256Hash,
    SignedAuthorization, UploadError,
};

/// The scripted HTTP/1.1 test double shared with
/// `mirror_delete_list_contract.rs` (#551); see `tests/support/mod.rs`
/// for the #538 full-request-read discipline it enforces.
mod support;

use support::{MockServer, ScriptedResponse};

fn loopback_allowlist() -> BTreeSet<String> {
    BTreeSet::from(["127.0.0.1".to_string()])
}

fn loopback_client() -> BlossomClient {
    BlossomClient::new(BlossomClientConfig {
        allowed_local_hosts: loopback_allowlist(),
        ..BlossomClientConfig::default()
    })
    .expect("client construction")
}

/// Draft -> sign -> validate, the exact production path an app takes
/// (the crate never signs; test keys stand in for `nmp-signer`).
fn signed_upload_auth(keys: &Keys, blob: &[u8], now: Timestamp) -> SignedAuthorization {
    let hash = Sha256Hash::of(blob);
    let draft = upload_authorization_draft(
        keys.public_key(),
        hash,
        Timestamp::from(now.as_secs() - 5),
        Timestamp::from(now.as_secs() + 600),
        "upload a test blob",
    )
    .expect("a future expiration");
    let event = draft.sign_with_keys(keys).expect("test signing");
    SignedAuthorization::validate(
        event,
        &ExpectedAuthorization {
            verb: BlossomVerb::Upload,
            blob: Some(hash),
        },
        now,
    )
    .expect("freshly built authorization validates")
}

fn descriptor_json_for(blob: &[u8]) -> Vec<u8> {
    let hex = Sha256Hash::of(blob).to_hex();
    format!(
        r#"{{"url":"https://cdn.example.com/{hex}","sha256":"{hex}","size":{},"type":"application/octet-stream","uploaded":1700000000}}"#,
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

/// Falsifier 1 (#545): the Authorization header the SERVER observes is a
/// valid kind:24242 event binding EXACTLY this blob's sha256, the `upload`
/// verb, and a future expiration -- and `validate()` refuses every
/// mis-binding: wrong blob, wrong verb, missing expiration, past
/// expiration, and a post-signing tamper.
#[tokio::test]
async fn upload_authorization_binds_exact_sha256_verb_and_expiration() {
    let blob = b"falsifier-one blob bytes";
    let hash = Sha256Hash::of(blob);
    let now = Timestamp::now();
    let keys = Keys::generate();
    let auth = signed_upload_auth(&keys, blob, now);

    let mock = MockServer::serve_one(ok_response(descriptor_json_for(blob)));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let client = loopback_client();
    let verified = client
        .upload(&server, blob, Some("application/octet-stream"), &auth)
        .await
        .expect("upload succeeds");
    assert_eq!(verified.descriptor().sha256, hash);

    let requests = mock.join();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "PUT");
    assert_eq!(request.path, "/upload");
    assert_eq!(request.body, blob);
    assert_eq!(request.headers.get("x-sha-256"), Some(&hash.to_hex()));
    assert_eq!(
        request.headers.get("content-type"),
        Some(&"application/octet-stream".to_string())
    );
    assert_eq!(
        request.headers.get("content-length"),
        Some(&blob.len().to_string())
    );

    // Decode the server-observed header back into the exact signed event.
    let authorization = request
        .headers
        .get("authorization")
        .expect("Authorization header sent");
    let payload = authorization
        .strip_prefix("Nostr ")
        .expect("BUD-11 `Nostr` scheme");
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .expect("base64url without padding");
    let observed = Event::from_json(&decoded).expect("event canonical JSON");
    assert!(observed.verify().is_ok());
    assert_eq!(observed.kind, Kind::BlossomAuth);
    assert_eq!(observed.kind.as_u16(), 24242);
    assert_eq!(
        observed
            .tags
            .find(TagKind::t())
            .and_then(|tag| tag.content()),
        Some("upload")
    );
    let x_values: Vec<String> = observed
        .tags
        .filter(TagKind::single_letter(Alphabet::X, false))
        .filter_map(|tag| tag.content())
        .map(str::to_string)
        .collect();
    assert_eq!(x_values, vec![hash.to_hex()]);
    let expiration = *observed.tags.expiration().expect("expiration tag");
    assert!(expiration > now);

    // validate() refuses an authorization for blob B used with blob A.
    let other_hash = Sha256Hash::of(b"a DIFFERENT blob");
    let bound_to_other = upload_authorization_draft(
        keys.public_key(),
        other_hash,
        Timestamp::from(now.as_secs() - 5),
        Timestamp::from(now.as_secs() + 600),
        "upload the other blob",
    )
    .expect("valid window")
    .sign_with_keys(&keys)
    .expect("test signing");
    let err = SignedAuthorization::validate(
        bound_to_other,
        &ExpectedAuthorization {
            verb: BlossomVerb::Upload,
            blob: Some(hash),
        },
        now,
    )
    .expect_err("blob B's authorization must not validate for blob A");
    assert_eq!(err, AuthValidationError::BlobNotBound { expected: hash });

    // validate() refuses a `delete` authorization for an upload expectation.
    let delete_auth = EventBuilder::new(Kind::BlossomAuth, "delete a blob")
        .tag(Tag::hashtag(BlossomVerb::Delete.as_tag_value()))
        .tag(Tag::custom(
            TagKind::single_letter(Alphabet::X, false),
            [hash.to_hex()],
        ))
        .tag(Tag::expiration(Timestamp::from(now.as_secs() + 600)))
        .custom_created_at(Timestamp::from(now.as_secs() - 5))
        .build(keys.public_key())
        .sign_with_keys(&keys)
        .expect("test signing");
    let err = SignedAuthorization::validate(
        delete_auth,
        &ExpectedAuthorization {
            verb: BlossomVerb::Upload,
            blob: Some(hash),
        },
        now,
    )
    .expect_err("a delete authorization must not validate for an upload");
    assert_eq!(
        err,
        AuthValidationError::VerbMismatch {
            expected: BlossomVerb::Upload,
            found: "delete".to_string(),
        }
    );

    // validate() refuses an authorization with no expiration tag.
    let no_expiration = EventBuilder::new(Kind::BlossomAuth, "upload a blob")
        .tag(Tag::hashtag(BlossomVerb::Upload.as_tag_value()))
        .tag(Tag::custom(
            TagKind::single_letter(Alphabet::X, false),
            [hash.to_hex()],
        ))
        .custom_created_at(Timestamp::from(now.as_secs() - 5))
        .build(keys.public_key())
        .sign_with_keys(&keys)
        .expect("test signing");
    let err = SignedAuthorization::validate(
        no_expiration,
        &ExpectedAuthorization {
            verb: BlossomVerb::Upload,
            blob: Some(hash),
        },
        now,
    )
    .expect_err("an expirationless authorization must be refused");
    assert_eq!(err, AuthValidationError::MissingExpiration);

    // validate() refuses an already-expired authorization.
    let expired = Timestamp::from(now.as_secs() - 10);
    let expired_auth = upload_authorization_draft(
        keys.public_key(),
        hash,
        Timestamp::from(now.as_secs() - 100),
        expired,
        "upload a blob",
    )
    .expect("expiration after created_at, both past")
    .sign_with_keys(&keys)
    .expect("test signing");
    let err = SignedAuthorization::validate(
        expired_auth,
        &ExpectedAuthorization {
            verb: BlossomVerb::Upload,
            blob: Some(hash),
        },
        now,
    )
    .expect_err("an already-expired authorization must be refused");
    assert_eq!(
        err,
        AuthValidationError::Expired {
            expiration: expired,
            now,
        }
    );

    // validate() refuses a tag tampered AFTER signing (signature check).
    let mut tampered = upload_authorization_draft(
        keys.public_key(),
        hash,
        Timestamp::from(now.as_secs() - 5),
        Timestamp::from(now.as_secs() + 600),
        "upload a blob",
    )
    .expect("valid window")
    .sign_with_keys(&keys)
    .expect("test signing");
    let mut tags = tampered.tags.clone().to_vec();
    tags.push(Tag::hashtag("sneaky-post-signing-tag"));
    tampered.tags = Tags::from_list(tags);
    let err = SignedAuthorization::validate(
        tampered,
        &ExpectedAuthorization {
            verb: BlossomVerb::Upload,
            blob: Some(hash),
        },
        now,
    )
    .expect_err("a post-signing tag tamper must be refused");
    assert!(matches!(err, AuthValidationError::BadSignature { .. }));

    // And the CLIENT refuses to send blob A under blob B's authorization
    // before any admission or socket I/O (step 1 of the upload contract).
    let auth_for_other = {
        let other_blob = b"a DIFFERENT blob";
        signed_upload_auth(&keys, other_blob, now)
    };
    let unused_server = BlossomServerUrl::parse("https://cdn.example.com").expect("public url");
    let err = client
        .upload(&unused_server, blob, None, &auth_for_other)
        .await
        .expect_err("mismatched authorization must be refused");
    assert_eq!(
        err,
        UploadError::AuthorizationBlobMismatch {
            expected: hash,
            authorized_verb: BlossomVerb::Upload,
            authorized_blob: Some(other_hash),
        }
    );
}

/// Falsifier 2 (#545): a 200 response whose descriptor names a DIFFERENT
/// sha256 than the uploaded bytes fails closed as `Sha256Mismatch` -- no
/// `VerifiedUpload` value can exist for it.
#[tokio::test]
async fn tampered_returned_descriptor_fails_closed() {
    let blob = b"falsifier-two blob bytes";
    let hash = Sha256Hash::of(blob);
    let other = Sha256Hash::of(b"substituted content");
    let now = Timestamp::now();
    let keys = Keys::generate();
    let auth = signed_upload_auth(&keys, blob, now);

    let tampered_descriptor = format!(
        r#"{{"url":"https://cdn.example.com/{other_hex}","sha256":"{other_hex}","size":{}}}"#,
        blob.len(),
        other_hex = other.to_hex()
    )
    .into_bytes();
    let mock = MockServer::serve_one(ok_response(tampered_descriptor));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let client = loopback_client();
    let err = client
        .upload(&server, blob, None, &auth)
        .await
        .expect_err("mismatched descriptor must fail closed");
    assert_eq!(
        err,
        UploadError::Sha256Mismatch {
            expected: hash,
            returned: other,
        }
    );
    mock.join();
}

/// Falsifier 3 (#545): `upload_authorization_draft` refuses an expiration
/// at or before `created_at` -- an authorization expired at birth can
/// never be composed, let alone signed.
#[test]
fn expired_or_inverted_expiration_is_refused_at_build_time() {
    let keys = Keys::generate();
    let hash = Sha256Hash::of(b"blob");
    let created_at = Timestamp::from(1_700_000_000u64);

    for expiration in [
        created_at,                                 // equal
        Timestamp::from(created_at.as_secs() - 60), // inverted
    ] {
        assert_eq!(
            upload_authorization_draft(
                keys.public_key(),
                hash,
                created_at,
                expiration,
                "upload a blob",
            ),
            Err(AuthDraftError::ExpirationNotAfterCreatedAt {
                created_at,
                expiration,
            })
        );
    }
}

/// Falsifier 4 (#545): with an EMPTY allowlist the upload to 127.0.0.1 is
/// refused before ANY socket I/O (the mock records ZERO accepted
/// connections); opting "127.0.0.1" in makes the very same upload succeed
/// end-to-end against the very same listener.
#[tokio::test]
async fn unadmitted_local_host_is_refused_before_any_socket_io() {
    let blob = b"falsifier-four blob bytes";
    let now = Timestamp::now();
    let keys = Keys::generate();
    let auth = signed_upload_auth(&keys, blob, now);

    let mock = MockServer::serve_one(ok_response(descriptor_json_for(blob)));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");

    let default_deny = BlossomClient::new(BlossomClientConfig::default()).expect("client");
    let err = default_deny
        .upload(&server, blob, None, &auth)
        .await
        .expect_err("unadmitted loopback must be refused");
    assert_eq!(
        err,
        UploadError::LocalHostNotAdmitted {
            host: "127.0.0.1".to_string(),
        }
    );
    assert_eq!(
        mock.accepted.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the refusal must happen before any socket I/O"
    );

    let opted_in = loopback_client();
    opted_in
        .upload(&server, blob, None, &auth)
        .await
        .expect("opted-in loopback upload succeeds");
    let requests = mock.join();
    assert_eq!(requests.len(), 1);
}

/// Names every [`UploadError`] variant with NO wildcard arm: adding a
/// variant without extending the taxonomy is a compile error.
fn taxonomy_slot(error: &UploadError) -> &'static str {
    match error {
        UploadError::AuthorizationBlobMismatch { .. } => "authorization-blob-mismatch",
        UploadError::LocalHostNotAdmitted { .. } => "local-host-not-admitted",
        UploadError::Network { .. } => "network",
        UploadError::RedirectRefused { .. } => "redirect-refused",
        UploadError::AuthRejected { .. } => "auth-rejected",
        UploadError::ServerRejected { .. } => "server-rejected",
        UploadError::ServerError { .. } => "server-error",
        UploadError::ResponseTooLarge { .. } => "response-too-large",
        UploadError::DescriptorInvalid(_) => "descriptor-invalid",
        UploadError::Sha256Mismatch { .. } => "sha256-mismatch",
    }
}

/// Falsifier 5 (#545): every HTTP outcome maps to its OWN taxonomy slot --
/// 401+X-Reason is `AuthRejected` carrying that exact reason, 404 is
/// `ServerRejected`, 500 is `ServerError`, 307 is `RedirectRefused` (never
/// followed), a dead port is `Network`, and a non-descriptor success body
/// is `DescriptorInvalid` -- while the wildcard-free `taxonomy_slot` match
/// proves exhaustiveness at compile time.
#[tokio::test]
async fn failure_taxonomy_is_separated_and_exhaustive() {
    let blob = b"falsifier-five blob bytes";
    let now = Timestamp::now();
    let keys = Keys::generate();
    let auth = signed_upload_auth(&keys, blob, now);
    let client = loopback_client();

    // 401 + X-Reason -> AuthRejected carrying that exact reason.
    let mock = MockServer::serve_one(ScriptedResponse {
        status_line: "HTTP/1.1 401 Unauthorized",
        extra_headers: vec![("X-Reason", "invalid nostr event".to_string())],
        body: Vec::new(),
    });
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let err = client
        .upload(&server, blob, None, &auth)
        .await
        .expect_err("401 must be AuthRejected");
    assert_eq!(
        err,
        UploadError::AuthRejected {
            status: 401,
            reason: Some("invalid nostr event".to_string()),
        }
    );
    assert_eq!(taxonomy_slot(&err), "auth-rejected");
    mock.join();

    // 404 -> ServerRejected (a non-auth, non-5xx refusal keeps its own slot).
    let mock = MockServer::serve_one(ScriptedResponse {
        status_line: "HTTP/1.1 404 Not Found",
        extra_headers: vec![("X-Reason", "unknown endpoint".to_string())],
        body: Vec::new(),
    });
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let err = client
        .upload(&server, blob, None, &auth)
        .await
        .expect_err("404 must be ServerRejected");
    assert_eq!(
        err,
        UploadError::ServerRejected {
            status: 404,
            reason: Some("unknown endpoint".to_string()),
        }
    );
    assert_eq!(taxonomy_slot(&err), "server-rejected");
    mock.join();

    // 500 -> ServerError.
    let mock = MockServer::serve_one(ScriptedResponse {
        status_line: "HTTP/1.1 500 Internal Server Error",
        extra_headers: Vec::new(),
        body: Vec::new(),
    });
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let err = client
        .upload(&server, blob, None, &auth)
        .await
        .expect_err("500 must be ServerError");
    assert_eq!(
        err,
        UploadError::ServerError {
            status: 500,
            reason: None,
        }
    );
    assert_eq!(taxonomy_slot(&err), "server-error");
    mock.join();

    // 307 -> RedirectRefused: redirects are disabled, so the 3xx arrives
    // as a response and is refused, never followed.
    let mock = MockServer::serve_one(ScriptedResponse {
        status_line: "HTTP/1.1 307 Temporary Redirect",
        extra_headers: vec![("Location", "http://127.0.0.1:1/upload".to_string())],
        body: Vec::new(),
    });
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let err = client
        .upload(&server, blob, None, &auth)
        .await
        .expect_err("307 must be RedirectRefused");
    assert_eq!(err, UploadError::RedirectRefused { status: 307 });
    assert_eq!(taxonomy_slot(&err), "redirect-refused");
    mock.join();

    // Connection refused (a bound-then-dropped port) -> Network.
    let dead_port = {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind then drop");
        listener.local_addr().expect("addr").port()
    };
    let server =
        BlossomServerUrl::parse(&format!("http://127.0.0.1:{dead_port}")).expect("dead url");
    let err = client
        .upload(&server, blob, None, &auth)
        .await
        .expect_err("a dead port must be Network");
    assert!(matches!(err, UploadError::Network { .. }));
    assert_eq!(taxonomy_slot(&err), "network");

    // A 200 whose body is not a descriptor -> DescriptorInvalid.
    let mock = MockServer::serve_one(ok_response(b"not-a-descriptor".to_vec()));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let err = client
        .upload(&server, blob, None, &auth)
        .await
        .expect_err("a non-descriptor success body must be DescriptorInvalid");
    assert!(matches!(err, UploadError::DescriptorInvalid(_)));
    assert_eq!(taxonomy_slot(&err), "descriptor-invalid");
    mock.join();
}

/// Falsifier 6 (#545): a success body larger than `max_response_bytes` is
/// refused as `ResponseTooLarge` while STREAMING -- the cap is enforced
/// chunk by chunk, never after buffering the whole body.
#[tokio::test]
async fn oversized_descriptor_response_is_bounded() {
    let blob = b"falsifier-six blob bytes";
    let now = Timestamp::now();
    let keys = Keys::generate();
    let auth = signed_upload_auth(&keys, blob, now);

    let client = BlossomClient::new(BlossomClientConfig {
        allowed_local_hosts: loopback_allowlist(),
        max_response_bytes: 256,
        ..BlossomClientConfig::default()
    })
    .expect("client construction");

    let mock = MockServer::serve_one(ok_response(vec![b'x'; 1000]));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let err = client
        .upload(&server, blob, None, &auth)
        .await
        .expect_err("an oversized body must be bounded");
    assert_eq!(err, UploadError::ResponseTooLarge { limit_bytes: 256 });
    mock.join();
}
