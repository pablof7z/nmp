//! #551 falsifiers for the BUD-04 mirror and BUD-12 delete/list
//! contracts: single-blob delete binding, client-side verb separation,
//! both forms of mirror hash failure, the 502-vs-5xx origin distinction,
//! strict bounded list parsing, and the three separated per-operation
//! failure taxonomies. Each test doc-comment names the invariant it would
//! falsify. (The post-DNS resolver falsifiers live as unit tests in
//! `src/client.rs`, where the private resolver hook is reachable.)

use std::collections::BTreeSet;
use std::net::TcpListener;

use nostr::{Alphabet, EventBuilder, Keys, Kind, Tag, TagKind, Timestamp};

use nmp_blossom::{
    delete_authorization_draft, list_authorization_draft, upload_authorization_draft,
    AuthValidationError, BlossomClient, BlossomClientConfig, BlossomServerUrl, BlossomVerb,
    DeleteError, DescriptorError, ExpectedAuthorization, ListError, ListPage, MirrorError,
    Sha256Hash, Sha256HexError, SignedAuthorization,
};

/// The scripted HTTP/1.1 test double shared with `upload_contract.rs`;
/// see `tests/support/mod.rs` for the #538 full-request-read discipline
/// it enforces.
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

/// Draft -> sign -> validate an `upload` grant for `blob` (BUD-04: this
/// is also the mirror authorization). The crate never signs; test keys
/// stand in for `nmp-signer`.
fn signed_upload_auth(keys: &Keys, blob: Sha256Hash, now: Timestamp) -> SignedAuthorization {
    let draft = upload_authorization_draft(
        keys.public_key(),
        blob,
        Timestamp::from(now.as_secs() - 5),
        Timestamp::from(now.as_secs() + 600),
        "authorize an upload or mirror",
    )
    .expect("a future expiration");
    let event = draft.sign_with_keys(keys).expect("test signing");
    SignedAuthorization::validate(
        event,
        &ExpectedAuthorization {
            verb: BlossomVerb::Upload,
            blob: Some(blob),
        },
        now,
    )
    .expect("freshly built authorization validates")
}

/// Draft -> sign -> validate a `delete` grant for `blob`.
fn signed_delete_auth(keys: &Keys, blob: Sha256Hash, now: Timestamp) -> SignedAuthorization {
    let draft = delete_authorization_draft(
        keys.public_key(),
        blob,
        Timestamp::from(now.as_secs() - 5),
        Timestamp::from(now.as_secs() + 600),
        "authorize a delete",
    )
    .expect("a future expiration");
    let event = draft.sign_with_keys(keys).expect("test signing");
    SignedAuthorization::validate(
        event,
        &ExpectedAuthorization {
            verb: BlossomVerb::Delete,
            blob: Some(blob),
        },
        now,
    )
    .expect("freshly built authorization validates")
}

/// Draft -> sign -> validate a `list` grant (no blob binding).
fn signed_list_auth(keys: &Keys, now: Timestamp) -> SignedAuthorization {
    let draft = list_authorization_draft(
        keys.public_key(),
        Timestamp::from(now.as_secs() - 5),
        Timestamp::from(now.as_secs() + 600),
        "authorize a list",
    )
    .expect("a future expiration");
    let event = draft.sign_with_keys(keys).expect("test signing");
    SignedAuthorization::validate(
        event,
        &ExpectedAuthorization {
            verb: BlossomVerb::List,
            blob: None,
        },
        now,
    )
    .expect("freshly built authorization validates")
}

fn descriptor_json(hash: Sha256Hash, size: u64, uploaded: u64) -> String {
    format!(
        r#"{{"url":"https://cdn.example.com/{hex}","sha256":"{hex}","size":{size},"uploaded":{uploaded}}}"#,
        hex = hash.to_hex()
    )
}

fn json_response(status_line: &'static str, body: Vec<u8>) -> ScriptedResponse {
    ScriptedResponse {
        status_line,
        extra_headers: vec![("Content-Type", "application/json".to_string())],
        body,
    }
}

fn empty_response(status_line: &'static str) -> ScriptedResponse {
    ScriptedResponse {
        status_line,
        extra_headers: Vec::new(),
        body: Vec::new(),
    }
}

fn reason_response(status_line: &'static str, reason: &str) -> ScriptedResponse {
    ScriptedResponse {
        status_line,
        extra_headers: vec![("X-Reason", reason.to_string())],
        body: Vec::new(),
    }
}

/// Names every [`MirrorError`] variant with NO wildcard arm: adding a
/// variant without extending the taxonomy is a compile error.
fn mirror_taxonomy_slot(error: &MirrorError) -> &'static str {
    match error {
        MirrorError::AuthorizationBlobMismatch { .. } => "authorization-blob-mismatch",
        MirrorError::LocalHostNotAdmitted { .. } => "local-host-not-admitted",
        MirrorError::Network { .. } => "network",
        MirrorError::RedirectRefused { .. } => "redirect-refused",
        MirrorError::AuthRejected { .. } => "auth-rejected",
        MirrorError::HashMismatchRefused { .. } => "hash-mismatch-refused",
        MirrorError::OriginFetchFailed { .. } => "origin-fetch-failed",
        MirrorError::ServerRejected { .. } => "server-rejected",
        MirrorError::ServerError { .. } => "server-error",
        MirrorError::ResponseTooLarge { .. } => "response-too-large",
        MirrorError::DescriptorInvalid(_) => "descriptor-invalid",
        MirrorError::Sha256Mismatch { .. } => "sha256-mismatch",
    }
}

/// Names every [`DeleteError`] variant with NO wildcard arm.
fn delete_taxonomy_slot(error: &DeleteError) -> &'static str {
    match error {
        DeleteError::AuthorizationBlobMismatch { .. } => "authorization-blob-mismatch",
        DeleteError::LocalHostNotAdmitted { .. } => "local-host-not-admitted",
        DeleteError::Network { .. } => "network",
        DeleteError::RedirectRefused { .. } => "redirect-refused",
        DeleteError::AuthRejected { .. } => "auth-rejected",
        DeleteError::NotFound { .. } => "not-found",
        DeleteError::ServerRejected { .. } => "server-rejected",
        DeleteError::ServerError { .. } => "server-error",
    }
}

/// Names every [`ListError`] variant with NO wildcard arm.
fn list_taxonomy_slot(error: &ListError) -> &'static str {
    match error {
        ListError::WrongVerb { .. } => "wrong-verb",
        ListError::LocalHostNotAdmitted { .. } => "local-host-not-admitted",
        ListError::Network { .. } => "network",
        ListError::RedirectRefused { .. } => "redirect-refused",
        ListError::AuthRejected { .. } => "auth-rejected",
        ListError::ServerRejected { .. } => "server-rejected",
        ListError::ServerError { .. } => "server-error",
        ListError::ResponseTooLarge { .. } => "response-too-large",
        ListError::BodyNotAnArray { .. } => "body-not-an-array",
        ListError::InvalidDescriptor { .. } => "invalid-descriptor",
    }
}

/// Falsifier a (#551, BUD-12): a delete authorization binds EXACTLY ONE
/// blob. A `delete` grant witnessed for blob A is refused client-side --
/// before ANY socket I/O (the mock records zero accepts) -- when used to
/// delete blob B; and a signed token carrying extra `x` tags never widens
/// the grant, because `validate()` witnesses ONE expected hash and
/// `delete()` compares that witness to the path hash. The same
/// authorization then deletes its own blob against the very same
/// listener, proving the refusals were the binding and nothing else.
#[tokio::test]
async fn delete_authorization_binds_exactly_one_blob() {
    let now = Timestamp::now();
    let keys = Keys::generate();
    let hash_a = Sha256Hash::of(b"blob A -- the one the grant binds");
    let hash_b = Sha256Hash::of(b"blob B -- never granted");

    let mock = MockServer::serve_one(empty_response("HTTP/1.1 200 OK"));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let client = loopback_client();

    // A's grant must not delete B.
    let auth_a = signed_delete_auth(&keys, hash_a, now);
    let err = client
        .delete(&server, hash_b, &auth_a)
        .await
        .expect_err("blob A's delete grant must not delete blob B");
    assert_eq!(
        err,
        DeleteError::AuthorizationBlobMismatch {
            expected: hash_b,
            authorized_verb: BlossomVerb::Delete,
            authorized_blob: Some(hash_a),
        }
    );
    assert_eq!(delete_taxonomy_slot(&err), "authorization-blob-mismatch");

    // A signed token carrying x=A AND x=B, validated for A, still deletes
    // ONLY A: the extra tag never widens the witnessed grant (BUD-12:
    // multiple `x` tags MUST NOT mean "delete multiple blobs").
    let two_blob_event = EventBuilder::new(Kind::BlossomAuth, "delete with an extra x tag")
        .tag(Tag::hashtag(BlossomVerb::Delete.as_tag_value()))
        .tag(Tag::custom(
            TagKind::single_letter(Alphabet::X, false),
            [hash_a.to_hex()],
        ))
        .tag(Tag::custom(
            TagKind::single_letter(Alphabet::X, false),
            [hash_b.to_hex()],
        ))
        .tag(Tag::expiration(Timestamp::from(now.as_secs() + 600)))
        .custom_created_at(Timestamp::from(now.as_secs() - 5))
        .build(keys.public_key())
        .sign_with_keys(&keys)
        .expect("test signing");
    let auth_widened = SignedAuthorization::validate(
        two_blob_event,
        &ExpectedAuthorization {
            verb: BlossomVerb::Delete,
            blob: Some(hash_a),
        },
        now,
    )
    .expect("x=A binds despite the extra tag (spec-sanctioned multiplicity)");
    let err = client
        .delete(&server, hash_b, &auth_widened)
        .await
        .expect_err("an extra x tag must not widen the witnessed single-blob grant");
    assert_eq!(
        err,
        DeleteError::AuthorizationBlobMismatch {
            expected: hash_b,
            authorized_verb: BlossomVerb::Delete,
            authorized_blob: Some(hash_a),
        }
    );

    // Both refusals happened before ANY socket I/O.
    assert_eq!(
        mock.accepted.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "mis-bound deletes must be refused before any socket I/O"
    );

    // The very same grant deletes its own blob end to end.
    client
        .delete(&server, hash_a, &auth_a)
        .await
        .expect("deleting the granted blob succeeds");
    let requests = mock.join();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "DELETE");
    assert_eq!(request.path, format!("/{}", hash_a.to_hex()));
    assert!(request.headers.contains_key("authorization"));
    assert!(request.body.is_empty());
}

/// Falsifier b (#551): verbs never cross operations, refused CLIENT-SIDE
/// before any admission or I/O -- an `upload` grant is refused by
/// `delete()`, a `delete` grant is refused by `mirror()`, an `upload`
/// grant is refused by `list()`, and a `list` expectation refuses an
/// upload-verb event inside `validate()` itself.
#[tokio::test]
async fn verb_confusion_is_refused_client_side() {
    let now = Timestamp::now();
    let keys = Keys::generate();
    let hash = Sha256Hash::of(b"verb confusion blob");
    let client = loopback_client();
    // A PUBLIC server URL: were any of these refusals to reach I/O they
    // would fail as Network against an unresolvable authority instead of
    // the typed mismatch asserted below.
    let server = BlossomServerUrl::parse("https://cdn.example.com").expect("public url");

    // upload grant -> delete(): refused.
    let upload_auth = signed_upload_auth(&keys, hash, now);
    let err = client
        .delete(&server, hash, &upload_auth)
        .await
        .expect_err("an upload grant must not authorize a delete");
    assert_eq!(
        err,
        DeleteError::AuthorizationBlobMismatch {
            expected: hash,
            authorized_verb: BlossomVerb::Upload,
            authorized_blob: Some(hash),
        }
    );

    // delete grant -> mirror(): refused.
    let delete_auth = signed_delete_auth(&keys, hash, now);
    let err = client
        .mirror(
            &server,
            "https://origin.example.com/blob",
            hash,
            &delete_auth,
        )
        .await
        .expect_err("a delete grant must not authorize a mirror");
    assert_eq!(
        err,
        MirrorError::AuthorizationBlobMismatch {
            expected: hash,
            authorized_verb: BlossomVerb::Delete,
            authorized_blob: Some(hash),
        }
    );
    assert_eq!(mirror_taxonomy_slot(&err), "authorization-blob-mismatch");

    // upload grant -> list(): refused as the typed WrongVerb.
    let err = client
        .list(
            &server,
            keys.public_key(),
            &ListPage::default(),
            Some(&upload_auth),
        )
        .await
        .expect_err("an upload grant must not authorize a list");
    assert_eq!(
        err,
        ListError::WrongVerb {
            authorized_verb: BlossomVerb::Upload,
        }
    );
    assert_eq!(list_taxonomy_slot(&err), "wrong-verb");

    // And validate() itself refuses an upload-verb event against a list
    // expectation -- the confusion never even becomes a SignedAuthorization.
    let upload_event = upload_authorization_draft(
        keys.public_key(),
        hash,
        Timestamp::from(now.as_secs() - 5),
        Timestamp::from(now.as_secs() + 600),
        "an upload, not a list",
    )
    .expect("a future expiration")
    .sign_with_keys(&keys)
    .expect("test signing");
    let err = SignedAuthorization::validate(
        upload_event,
        &ExpectedAuthorization {
            verb: BlossomVerb::List,
            blob: None,
        },
        now,
    )
    .expect_err("an upload-verb event must not validate for a list expectation");
    assert_eq!(
        err,
        AuthValidationError::VerbMismatch {
            expected: BlossomVerb::List,
            found: "upload".to_string(),
        }
    );
}

/// Falsifier c (#551, BUD-04): BOTH forms of mirror hash mismatch fail
/// closed as their own variants -- a 409 is the SERVER's refusal
/// (`HashMismatchRefused`), while a 200 whose descriptor names a
/// different sha256 is THIS CLIENT's integrity gate (`Sha256Mismatch`),
/// and in neither case does a `VerifiedUpload` escape. Also pins the
/// BUD-04 wire shape: PUT /mirror, JSON `{"url": ...}` body, and the
/// Authorization header.
#[tokio::test]
async fn mirror_hash_mismatch_fails_closed_in_both_forms() {
    let now = Timestamp::now();
    let keys = Keys::generate();
    let expected = Sha256Hash::of(b"the blob being mirrored");
    let auth = signed_upload_auth(&keys, expected, now);
    let client = loopback_client();
    let source_url = "https://origin.example.com/blob.png";

    // Form 1: the server verified the downloaded bytes and refused (409).
    let mock = MockServer::serve_one(reason_response(
        "HTTP/1.1 409 Conflict",
        "mirrored blob hash does not match authorization",
    ));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let err = client
        .mirror(&server, source_url, expected, &auth)
        .await
        .expect_err("a 409 must surface as the server's hash refusal");
    assert_eq!(
        err,
        MirrorError::HashMismatchRefused {
            reason: Some("mirrored blob hash does not match authorization".to_string()),
        }
    );
    assert_eq!(mirror_taxonomy_slot(&err), "hash-mismatch-refused");
    let requests = mock.join();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "PUT");
    assert_eq!(request.path, "/mirror");
    assert_eq!(
        request.headers.get("content-type"),
        Some(&"application/json".to_string())
    );
    assert!(request.headers.contains_key("authorization"));
    assert_eq!(
        request.body,
        format!(r#"{{"url":"{source_url}"}}"#).into_bytes()
    );

    // Form 2: the server claims success but its descriptor names a
    // DIFFERENT sha256 -- the client's own gate refuses it.
    let other = Sha256Hash::of(b"substituted content");
    let mock = MockServer::serve_one(json_response(
        "HTTP/1.1 200 OK",
        descriptor_json(other, 21, 1_700_000_000).into_bytes(),
    ));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let err = client
        .mirror(&server, source_url, expected, &auth)
        .await
        .expect_err("a tampered 200 descriptor must fail the integrity gate");
    assert_eq!(
        err,
        MirrorError::Sha256Mismatch {
            expected,
            returned: other,
        }
    );
    assert_eq!(mirror_taxonomy_slot(&err), "sha256-mismatch");
    mock.join();
}

/// Falsifier d (#551, BUD-04): a 502 ("could not fetch the origin URL")
/// is `OriginFetchFailed`, provably DISTINCT from a 500 `ServerError`
/// carrying the very same reason -- the 502 arm matches before the
/// generic 5xx arm, so callers can tell "the origin is broken" from "the
/// destination is broken".
#[tokio::test]
async fn mirror_origin_fetch_failure_is_distinct() {
    let now = Timestamp::now();
    let keys = Keys::generate();
    let expected = Sha256Hash::of(b"origin fetch blob");
    let auth = signed_upload_auth(&keys, expected, now);
    let client = loopback_client();
    let source_url = "https://origin.example.com/blob.png";

    let mock = MockServer::serve_one(reason_response(
        "HTTP/1.1 502 Bad Gateway",
        "origin unreachable",
    ));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let origin_failure = client
        .mirror(&server, source_url, expected, &auth)
        .await
        .expect_err("502 must be OriginFetchFailed");
    assert_eq!(
        origin_failure,
        MirrorError::OriginFetchFailed {
            reason: Some("origin unreachable".to_string()),
        }
    );
    mock.join();

    let mock = MockServer::serve_one(reason_response(
        "HTTP/1.1 500 Internal Server Error",
        "origin unreachable",
    ));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let server_failure = client
        .mirror(&server, source_url, expected, &auth)
        .await
        .expect_err("500 must be ServerError");
    assert_eq!(
        server_failure,
        MirrorError::ServerError {
            status: 500,
            reason: Some("origin unreachable".to_string()),
        }
    );
    mock.join();

    // Same reason text, different variants: the distinction is the
    // taxonomy, not the payload.
    assert_ne!(origin_failure, server_failure);
    assert_eq!(mirror_taxonomy_slot(&origin_failure), "origin-fetch-failed");
    assert_eq!(mirror_taxonomy_slot(&server_failure), "server-error");
}

/// Falsifier e (#551, BUD-12): list parsing is bounded and strict -- an
/// oversized array body is refused WHILE STREAMING as `ResponseTooLarge`;
/// a non-array body is `BodyNotAnArray`; an array whose second row fails
/// the strict sha256 rules is `InvalidDescriptor { index: 1 }` and NEVER
/// a one-row success; and the happy path returns descriptors in server
/// order with `cursor`/`limit` visible in the recorded query string (and
/// no Authorization header when no auth is supplied).
#[tokio::test]
async fn list_parsing_is_bounded_and_strict() {
    let now = Timestamp::now();
    let keys = Keys::generate();
    let owner = keys.public_key();
    let client = loopback_client();

    // Oversized body -> ResponseTooLarge under the LIST cap.
    let capped_client = BlossomClient::new(BlossomClientConfig {
        allowed_local_hosts: loopback_allowlist(),
        max_list_response_bytes: 256,
        ..BlossomClientConfig::default()
    })
    .expect("client construction");
    let mock = MockServer::serve_one(json_response("HTTP/1.1 200 OK", vec![b'x'; 1000]));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let err = capped_client
        .list(&server, owner, &ListPage::default(), None)
        .await
        .expect_err("an oversized list body must be bounded");
    assert_eq!(err, ListError::ResponseTooLarge { limit_bytes: 256 });
    assert_eq!(list_taxonomy_slot(&err), "response-too-large");
    mock.join();

    // A non-array top level is refused outright.
    let mock = MockServer::serve_one(json_response(
        "HTTP/1.1 200 OK",
        b"{\"not\":\"an array\"}".to_vec(),
    ));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let err = client
        .list(&server, owner, &ListPage::default(), None)
        .await
        .expect_err("a non-array list body must be refused");
    assert!(matches!(err, ListError::BodyNotAnArray { .. }));
    assert_eq!(list_taxonomy_slot(&err), "body-not-an-array");
    mock.join();

    // One malformed row (bad sha256 hex at index 1) fails the WHOLE call
    // typed -- never a silently truncated one-row success.
    let good = Sha256Hash::of(b"list blob one");
    let body = format!(
        r#"[{},{{"url":"https://cdn.example.com/x","sha256":"NOT-HEX","size":4}}]"#,
        descriptor_json(good, 13, 1_700_000_100)
    );
    let mock = MockServer::serve_one(json_response("HTTP/1.1 200 OK", body.into_bytes()));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let err = client
        .list(&server, owner, &ListPage::default(), None)
        .await
        .expect_err("a malformed row must fail the whole list");
    assert_eq!(
        err,
        ListError::InvalidDescriptor {
            index: 1,
            source: DescriptorError::BadSha256(Sha256HexError::BadLength { length: 7 }),
        }
    );
    assert_eq!(list_taxonomy_slot(&err), "invalid-descriptor");
    mock.join();

    // Happy path: descriptors in server order; cursor+limit ride the
    // query string; the list authorization rides the header.
    let newer = Sha256Hash::of(b"list blob newer");
    let older = Sha256Hash::of(b"list blob older");
    let cursor = Sha256Hash::of(b"previous page's last blob");
    let body = format!(
        "[{},{}]",
        descriptor_json(newer, 15, 1_700_000_200),
        descriptor_json(older, 15, 1_700_000_100)
    );
    let mock = MockServer::serve_one(json_response("HTTP/1.1 200 OK", body.into_bytes()));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let auth = signed_list_auth(&keys, now);
    let descriptors = client
        .list(
            &server,
            owner,
            &ListPage {
                cursor: Some(cursor),
                limit: Some(25),
            },
            Some(&auth),
        )
        .await
        .expect("a well-formed list succeeds");
    assert_eq!(descriptors.len(), 2);
    assert_eq!(descriptors[0].sha256, newer);
    assert_eq!(descriptors[0].uploaded, Some(1_700_000_200));
    assert_eq!(descriptors[1].sha256, older);
    assert_eq!(descriptors[1].uploaded, Some(1_700_000_100));
    let requests = mock.join();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "GET");
    assert_eq!(
        request.path,
        format!(
            "/list/{}?cursor={}&limit=25",
            owner.to_hex(),
            cursor.to_hex()
        )
    );
    assert!(request.headers.contains_key("authorization"));

    // Auth-less list: no Authorization header, no query parameters.
    let body = format!("[{}]", descriptor_json(newer, 15, 1_700_000_200));
    let mock = MockServer::serve_one(json_response("HTTP/1.1 200 OK", body.into_bytes()));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let descriptors = client
        .list(&server, owner, &ListPage::default(), None)
        .await
        .expect("an auth-less list succeeds when the server allows it");
    assert_eq!(descriptors.len(), 1);
    let requests = mock.join();
    let request = &requests[0];
    assert_eq!(request.path, format!("/list/{}", owner.to_hex()));
    assert!(!request.headers.contains_key("authorization"));
}

/// Falsifier g1 (#551): every remaining [`MirrorError`] outcome maps to
/// its OWN taxonomy slot -- 307 is `RedirectRefused` (never followed),
/// 401+X-Reason is `AuthRejected` carrying that exact reason, 415 is
/// `ServerRejected`, a dead port is `Network`, an unadmitted literal
/// private host is refused pre-socket, an oversized 200 body is bounded,
/// and a non-descriptor 200 body is `DescriptorInvalid` -- while the
/// wildcard-free `mirror_taxonomy_slot` match proves exhaustiveness at
/// compile time (409/502/500/mismatch slots are pinned by falsifiers
/// c/d, blob mismatch by falsifier b).
#[tokio::test]
async fn mirror_failure_taxonomy_is_separated_and_exhaustive() {
    let now = Timestamp::now();
    let keys = Keys::generate();
    let expected = Sha256Hash::of(b"mirror taxonomy blob");
    let auth = signed_upload_auth(&keys, expected, now);
    let client = loopback_client();
    let source_url = "https://origin.example.com/blob.png";

    // 307 -> RedirectRefused.
    let mock = MockServer::serve_one(ScriptedResponse {
        status_line: "HTTP/1.1 307 Temporary Redirect",
        extra_headers: vec![("Location", "http://127.0.0.1:1/mirror".to_string())],
        body: Vec::new(),
    });
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let err = client
        .mirror(&server, source_url, expected, &auth)
        .await
        .expect_err("307 must be RedirectRefused");
    assert_eq!(err, MirrorError::RedirectRefused { status: 307 });
    assert_eq!(mirror_taxonomy_slot(&err), "redirect-refused");
    mock.join();

    // 401 + X-Reason -> AuthRejected carrying that exact reason.
    let mock = MockServer::serve_one(reason_response(
        "HTTP/1.1 401 Unauthorized",
        "invalid nostr event",
    ));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let err = client
        .mirror(&server, source_url, expected, &auth)
        .await
        .expect_err("401 must be AuthRejected");
    assert_eq!(
        err,
        MirrorError::AuthRejected {
            status: 401,
            reason: Some("invalid nostr event".to_string()),
        }
    );
    assert_eq!(mirror_taxonomy_slot(&err), "auth-rejected");
    mock.join();

    // 415 -> ServerRejected (a non-auth, non-5xx refusal).
    let mock = MockServer::serve_one(reason_response(
        "HTTP/1.1 415 Unsupported Media Type",
        "type not allowed",
    ));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let err = client
        .mirror(&server, source_url, expected, &auth)
        .await
        .expect_err("415 must be ServerRejected");
    assert_eq!(
        err,
        MirrorError::ServerRejected {
            status: 415,
            reason: Some("type not allowed".to_string()),
        }
    );
    assert_eq!(mirror_taxonomy_slot(&err), "server-rejected");
    mock.join();

    // Connection refused (a bound-then-dropped port) -> Network.
    let dead_port = {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind then drop");
        listener.local_addr().expect("addr").port()
    };
    let server =
        BlossomServerUrl::parse(&format!("http://127.0.0.1:{dead_port}")).expect("dead url");
    let err = client
        .mirror(&server, source_url, expected, &auth)
        .await
        .expect_err("a dead port must be Network");
    assert!(matches!(err, MirrorError::Network { .. }));
    assert_eq!(mirror_taxonomy_slot(&err), "network");

    // An unadmitted literal private host -> refused pre-socket.
    let default_deny = BlossomClient::new(BlossomClientConfig::default()).expect("client");
    let server = BlossomServerUrl::parse("http://192.168.1.9").expect("private-host url");
    let err = default_deny
        .mirror(&server, source_url, expected, &auth)
        .await
        .expect_err("an unadmitted private host must be refused");
    assert_eq!(
        err,
        MirrorError::LocalHostNotAdmitted {
            host: "192.168.1.9".to_string(),
        }
    );
    assert_eq!(mirror_taxonomy_slot(&err), "local-host-not-admitted");

    // An oversized 200 body is bounded while streaming.
    let capped_client = BlossomClient::new(BlossomClientConfig {
        allowed_local_hosts: loopback_allowlist(),
        max_response_bytes: 256,
        ..BlossomClientConfig::default()
    })
    .expect("client construction");
    let mock = MockServer::serve_one(json_response("HTTP/1.1 200 OK", vec![b'x'; 1000]));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let err = capped_client
        .mirror(&server, source_url, expected, &auth)
        .await
        .expect_err("an oversized body must be bounded");
    assert_eq!(err, MirrorError::ResponseTooLarge { limit_bytes: 256 });
    assert_eq!(mirror_taxonomy_slot(&err), "response-too-large");
    mock.join();

    // A 200 whose body is not a descriptor -> DescriptorInvalid.
    let mock = MockServer::serve_one(json_response(
        "HTTP/1.1 200 OK",
        b"not-a-descriptor".to_vec(),
    ));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let err = client
        .mirror(&server, source_url, expected, &auth)
        .await
        .expect_err("a non-descriptor success body must be DescriptorInvalid");
    assert!(matches!(err, MirrorError::DescriptorInvalid(_)));
    assert_eq!(mirror_taxonomy_slot(&err), "descriptor-invalid");
    mock.join();
}

/// Falsifier g2 (#551, BUD-12): every remaining [`DeleteError`] outcome
/// maps to its OWN taxonomy slot -- 402 is `AuthRejected` (BUD-12 counts
/// payment-required as authorization-indicting for DELETE), 404 is the
/// distinct `NotFound`, 429 is `ServerRejected`, 503 is `ServerError`,
/// 307 is `RedirectRefused`, a dead port is `Network`, and an unadmitted
/// literal private host is refused pre-socket -- while the wildcard-free
/// `delete_taxonomy_slot` match proves exhaustiveness at compile time
/// (blob/verb mismatch slots are pinned by falsifiers a/b).
#[tokio::test]
async fn delete_failure_taxonomy_is_separated_and_exhaustive() {
    let now = Timestamp::now();
    let keys = Keys::generate();
    let blob = Sha256Hash::of(b"delete taxonomy blob");
    let auth = signed_delete_auth(&keys, blob, now);
    let client = loopback_client();

    // 402 -> AuthRejected keeping the exact status.
    let mock = MockServer::serve_one(reason_response(
        "HTTP/1.1 402 Payment Required",
        "insufficient balance",
    ));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let err = client
        .delete(&server, blob, &auth)
        .await
        .expect_err("402 must be AuthRejected");
    assert_eq!(
        err,
        DeleteError::AuthRejected {
            status: 402,
            reason: Some("insufficient balance".to_string()),
        }
    );
    assert_eq!(delete_taxonomy_slot(&err), "auth-rejected");
    mock.join();

    // 404 -> the distinct NotFound.
    let mock = MockServer::serve_one(reason_response("HTTP/1.1 404 Not Found", "no such blob"));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let err = client
        .delete(&server, blob, &auth)
        .await
        .expect_err("404 must be NotFound");
    assert_eq!(
        err,
        DeleteError::NotFound {
            reason: Some("no such blob".to_string()),
        }
    );
    assert_eq!(delete_taxonomy_slot(&err), "not-found");
    mock.join();

    // 429 -> ServerRejected (a non-auth, non-5xx refusal).
    let mock = MockServer::serve_one(empty_response("HTTP/1.1 429 Too Many Requests"));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let err = client
        .delete(&server, blob, &auth)
        .await
        .expect_err("429 must be ServerRejected");
    assert_eq!(
        err,
        DeleteError::ServerRejected {
            status: 429,
            reason: None,
        }
    );
    assert_eq!(delete_taxonomy_slot(&err), "server-rejected");
    mock.join();

    // 503 -> ServerError.
    let mock = MockServer::serve_one(empty_response("HTTP/1.1 503 Service Unavailable"));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let err = client
        .delete(&server, blob, &auth)
        .await
        .expect_err("503 must be ServerError");
    assert_eq!(
        err,
        DeleteError::ServerError {
            status: 503,
            reason: None,
        }
    );
    assert_eq!(delete_taxonomy_slot(&err), "server-error");
    mock.join();

    // 307 -> RedirectRefused: a delete is never re-aimed.
    let mock = MockServer::serve_one(ScriptedResponse {
        status_line: "HTTP/1.1 307 Temporary Redirect",
        extra_headers: vec![("Location", "http://127.0.0.1:1/".to_string())],
        body: Vec::new(),
    });
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let err = client
        .delete(&server, blob, &auth)
        .await
        .expect_err("307 must be RedirectRefused");
    assert_eq!(err, DeleteError::RedirectRefused { status: 307 });
    assert_eq!(delete_taxonomy_slot(&err), "redirect-refused");
    mock.join();

    // Connection refused -> Network.
    let dead_port = {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind then drop");
        listener.local_addr().expect("addr").port()
    };
    let server =
        BlossomServerUrl::parse(&format!("http://127.0.0.1:{dead_port}")).expect("dead url");
    let err = client
        .delete(&server, blob, &auth)
        .await
        .expect_err("a dead port must be Network");
    assert!(matches!(err, DeleteError::Network { .. }));
    assert_eq!(delete_taxonomy_slot(&err), "network");

    // An unadmitted literal private host -> refused pre-socket.
    let default_deny = BlossomClient::new(BlossomClientConfig::default()).expect("client");
    let server = BlossomServerUrl::parse("http://10.0.0.7").expect("private-host url");
    let err = default_deny
        .delete(&server, blob, &auth)
        .await
        .expect_err("an unadmitted private host must be refused");
    assert_eq!(
        err,
        DeleteError::LocalHostNotAdmitted {
            host: "10.0.0.7".to_string(),
        }
    );
    assert_eq!(delete_taxonomy_slot(&err), "local-host-not-admitted");
}

/// Falsifier g3 (#551, BUD-12): every remaining [`ListError`] outcome
/// maps to its OWN taxonomy slot -- a 401 on an AUTH-LESS call is
/// `AuthRejected` (the server may require a `list` grant), 404 is
/// `ServerRejected`, 500 is `ServerError`, 307 is `RedirectRefused`, a
/// dead port is `Network`, and an unadmitted `.localhost` name is refused
/// pre-socket -- while the wildcard-free `list_taxonomy_slot` match
/// proves exhaustiveness at compile time (wrong-verb by falsifier b;
/// bounded/strict parse slots by falsifier e).
#[tokio::test]
async fn list_failure_taxonomy_is_separated_and_exhaustive() {
    let keys = Keys::generate();
    let owner = keys.public_key();
    let client = loopback_client();

    // 401 on an auth-less call -> AuthRejected.
    let mock = MockServer::serve_one(reason_response(
        "HTTP/1.1 401 Unauthorized",
        "authorization required",
    ));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let err = client
        .list(&server, owner, &ListPage::default(), None)
        .await
        .expect_err("401 must be AuthRejected");
    assert_eq!(
        err,
        ListError::AuthRejected {
            status: 401,
            reason: Some("authorization required".to_string()),
        }
    );
    assert_eq!(list_taxonomy_slot(&err), "auth-rejected");
    mock.join();

    // 404 -> ServerRejected.
    let mock = MockServer::serve_one(empty_response("HTTP/1.1 404 Not Found"));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let err = client
        .list(&server, owner, &ListPage::default(), None)
        .await
        .expect_err("404 must be ServerRejected");
    assert_eq!(
        err,
        ListError::ServerRejected {
            status: 404,
            reason: None,
        }
    );
    assert_eq!(list_taxonomy_slot(&err), "server-rejected");
    mock.join();

    // 500 -> ServerError.
    let mock = MockServer::serve_one(empty_response("HTTP/1.1 500 Internal Server Error"));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let err = client
        .list(&server, owner, &ListPage::default(), None)
        .await
        .expect_err("500 must be ServerError");
    assert_eq!(
        err,
        ListError::ServerError {
            status: 500,
            reason: None,
        }
    );
    assert_eq!(list_taxonomy_slot(&err), "server-error");
    mock.join();

    // 307 -> RedirectRefused.
    let mock = MockServer::serve_one(ScriptedResponse {
        status_line: "HTTP/1.1 307 Temporary Redirect",
        extra_headers: vec![("Location", "http://127.0.0.1:1/list".to_string())],
        body: Vec::new(),
    });
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let err = client
        .list(&server, owner, &ListPage::default(), None)
        .await
        .expect_err("307 must be RedirectRefused");
    assert_eq!(err, ListError::RedirectRefused { status: 307 });
    assert_eq!(list_taxonomy_slot(&err), "redirect-refused");
    mock.join();

    // Connection refused -> Network.
    let dead_port = {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind then drop");
        listener.local_addr().expect("addr").port()
    };
    let server =
        BlossomServerUrl::parse(&format!("http://127.0.0.1:{dead_port}")).expect("dead url");
    let err = client
        .list(&server, owner, &ListPage::default(), None)
        .await
        .expect_err("a dead port must be Network");
    assert!(matches!(err, ListError::Network { .. }));
    assert_eq!(list_taxonomy_slot(&err), "network");

    // An unadmitted `.localhost` NAME (not an IP literal) -> refused
    // pre-socket by the same hostname rules as `nmp-transport`.
    let default_deny = BlossomClient::new(BlossomClientConfig::default()).expect("client");
    let server = BlossomServerUrl::parse("http://blossom.localhost").expect("localhost url");
    let err = default_deny
        .list(&server, owner, &ListPage::default(), None)
        .await
        .expect_err("an unadmitted .localhost name must be refused");
    assert_eq!(
        err,
        ListError::LocalHostNotAdmitted {
            host: "blossom.localhost".to_string(),
        }
    );
    assert_eq!(list_taxonomy_slot(&err), "local-host-not-admitted");
}
