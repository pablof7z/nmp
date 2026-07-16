//! #559 falsifiers for the staged composition seam (epic #216
//! T15-C-MEDIA-COMPOSITION): the three witness stages (prepare -> upload ->
//! compose) exercised end to end against a real async loopback Blossom mock,
//! the held-bytes substitution refusal, the three SEPARATE failure domains,
//! and compose determinism. Each test doc-comment names the invariant it
//! would falsify.

use nostr::{JsonUtil, Keys, Kind, Tag, Timestamp};

use nmp_blossom::{
    upload_authorization_draft, AuthDraftError, BlossomClient, BlossomClientConfig,
    BlossomServerUrl, BlossomVerb, ExpectedAuthorization, Sha256Hash, SignedAuthorization,
    UploadError,
};
use nmp_nip68::{ContentWarning, ImageDim, PictureBuildError, PictureImageError};

use nmp_media::{
    compose_picture, prepare, ComposedImage, MediaComposeError, MediaUploadError, PicturePost,
    PrepareError, PreparedUpload, UploadedAsset,
};

/// The scripted HTTP/1.1 loopback mock (raw TcpListener, one connection per
/// server, full-request-drain-before-reply -- the #538 lesson), reused from
/// `crates/nmp-blossom/tests/support`.
mod support;

use support::{MockServer, ScriptedResponse};

// --- shared helpers -------------------------------------------------------

fn loopback_client() -> BlossomClient {
    BlossomClient::new(BlossomClientConfig {
        allowed_local_hosts: std::collections::BTreeSet::from(["127.0.0.1".to_string()]),
        ..BlossomClientConfig::default()
    })
    .expect("client construction")
}

/// Sign a prepared upload's authorization draft with test keys (standing in
/// for `nmp-signer`, which this crate never invokes) and validate it into a
/// `SignedAuthorization` bound to the prepared hash -- the exact production
/// path an app takes between the prepare and upload stages.
fn sign_prepared(prepared: &PreparedUpload, keys: &Keys, now: Timestamp) -> SignedAuthorization {
    let event = prepared
        .authorization_draft()
        .clone()
        .sign_with_keys(keys)
        .expect("test signing");
    SignedAuthorization::validate(
        event,
        &ExpectedAuthorization {
            verb: BlossomVerb::Upload,
            blob: Some(prepared.sha256()),
        },
        now,
    )
    .expect("freshly prepared authorization validates")
}

/// A BUD-02 descriptor JSON body with a caller-chosen `url` and optional
/// `type` (mime), whose `sha256` matches `blob` -- so the client's integrity
/// gate passes and the returned `VerifiedUpload` carries `url`/`type`.
fn descriptor_body(blob: &[u8], url: &str, mime: Option<&str>) -> Vec<u8> {
    let hex = Sha256Hash::of(blob).to_hex();
    let mut json = format!(r#"{{"url":"{url}","sha256":"{hex}","size":{}"#, blob.len());
    if let Some(mime) = mime {
        json.push_str(&format!(r#","type":"{mime}""#));
    }
    json.push('}');
    json.into_bytes()
}

fn ok_response(body: Vec<u8>) -> ScriptedResponse {
    ScriptedResponse {
        status_line: "HTTP/1.1 200 OK",
        extra_headers: vec![("Content-Type", "application/json".to_string())],
        body,
    }
}

fn past(now: Timestamp) -> Timestamp {
    Timestamp::from(now.as_secs() - 5)
}

fn future(now: Timestamp) -> Timestamp {
    Timestamp::from(now.as_secs() + 600)
}

/// Run prepare -> sign -> upload against a fresh mock and return the verified
/// asset. `descriptor_mime` chooses whether the server's descriptor carries a
/// `type`, so compose-stage provenance can be falsified.
async fn upload_one(
    keys: &Keys,
    blob: &[u8],
    descriptor_mime: Option<&str>,
    now: Timestamp,
) -> UploadedAsset {
    let prepared = prepare(
        blob.to_vec(),
        "image/png",
        keys.public_key(),
        past(now),
        future(now),
        "upload a test image",
    )
    .expect("prepare");
    let auth = sign_prepared(&prepared, keys, now);
    let url = format!("https://cdn.example.com/{}", Sha256Hash::of(blob).to_hex());
    let mock = MockServer::serve_one(ok_response(descriptor_body(blob, &url, descriptor_mime)));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let client = loopback_client();
    let asset = prepared
        .upload(&client, &server, &auth)
        .await
        .expect("upload succeeds");
    mock.join();
    asset
}

fn tag_name(tag: &Tag) -> &str {
    tag.as_slice().first().map(String::as_str).unwrap_or("")
}

// --- falsifiers -----------------------------------------------------------

/// Falsifier 1 (#559): the three stages compose a kind:20 draft end to end
/// with a REAL async upload. prepare(bytes) -> sign its authorization draft
/// -> upload against the mock (a descriptor whose sha256 == hash of bytes) ->
/// compose_picture; the final `UnsignedEvent` is kind 20, its imeta `x` is the
/// hex sha256 of the bytes, and its imeta `url` is the mock descriptor's url.
#[tokio::test]
async fn staged_pipeline_composes_a_kind20_draft_end_to_end() {
    let blob = b"staged pipeline image bytes";
    let hash = Sha256Hash::of(blob);
    let now = Timestamp::now();
    let keys = Keys::generate();
    let author = keys.public_key();

    // Stage 1: prepare binds the exact bytes to the exact authorization draft.
    let prepared = prepare(
        blob.to_vec(),
        "image/png",
        author,
        past(now),
        future(now),
        "upload a test image",
    )
    .expect("prepare");
    assert_eq!(prepared.sha256(), hash);
    assert_eq!(prepared.mime_type(), "image/png");

    // The app signs the prepared draft (this crate never signs).
    let auth = sign_prepared(&prepared, &keys, now);

    // Stage 2: the real async upload of the HELD bytes.
    let descriptor_url = format!("https://cdn.example.com/{}", hash.to_hex());
    let mock = MockServer::serve_one(ok_response(descriptor_body(
        blob,
        &descriptor_url,
        Some("image/png"),
    )));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url");
    let client = loopback_client();
    let asset = prepared
        .upload(&client, &server, &auth)
        .await
        .expect("upload succeeds");
    assert_eq!(asset.sha256(), hash);
    assert_eq!(asset.descriptor().url, descriptor_url);

    // The server observed exactly one PUT /upload carrying the held bytes.
    let requests = mock.join();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "PUT");
    assert_eq!(requests[0].path, "/upload");
    assert_eq!(requests[0].body, blob);

    // Stage 3: compose the kind:20 draft.
    let post = PicturePost {
        title: Some("A title".to_string()),
        description: "hello world".to_string(),
        content_warning: None,
        hashtags: vec!["cats".to_string()],
    };
    let event = compose_picture(
        author,
        now,
        vec![ComposedImage::new(asset).with_dim(ImageDim {
            width: 100,
            height: 200,
        })],
        &post,
    )
    .expect("compose");

    assert_eq!(event.kind, Kind::from(20u16));
    assert_eq!(event.content, "hello world");
    let imeta = event
        .tags
        .iter()
        .find(|tag| tag_name(tag) == "imeta")
        .expect("the composed kind:20 carries an imeta row");
    let values = imeta.as_slice();
    assert!(values.iter().any(|v| v == &format!("x {}", hash.to_hex())));
    assert!(values.iter().any(|v| v == &format!("url {descriptor_url}")));
}

/// Falsifier 2 (#559): the seam uploads EXACTLY the prepared bytes -- an
/// authorization that binds the hash of DIFFERENT bytes is refused because the
/// HELD bytes hash to A, not the authorized B. The uploaded-bytes/authorized-
/// hash mismatch is structurally impossible to get wrong: the underlying
/// client re-hashes the held bytes and refuses with `AuthorizationBlobMismatch`
/// before any socket I/O. The upstream Blossom taxonomy crosses the seam
/// intact inside `MediaUploadError::Blossom`.
#[tokio::test]
async fn held_bytes_cannot_be_substituted() {
    let bytes_a = b"the bytes actually prepared and held";
    let bytes_b = b"totally different bytes the auth binds";
    let hash_a = Sha256Hash::of(bytes_a);
    let hash_b = Sha256Hash::of(bytes_b);
    let now = Timestamp::now();
    let keys = Keys::generate();
    let author = keys.public_key();

    let prepared = prepare(
        bytes_a.to_vec(),
        "image/png",
        author,
        past(now),
        future(now),
        "prepare bytes A",
    )
    .expect("prepare A");

    // A perfectly valid authorization -- but for bytes B, not the held A.
    let draft_b = upload_authorization_draft(author, hash_b, past(now), future(now), "authorize B")
        .expect("draft B");
    let event_b = draft_b.sign_with_keys(&keys).expect("test signing");
    let auth_b = SignedAuthorization::validate(
        event_b,
        &ExpectedAuthorization {
            verb: BlossomVerb::Upload,
            blob: Some(hash_b),
        },
        now,
    )
    .expect("authorization for B validates");

    let client = loopback_client();
    // Rejected before any I/O, so the server URL is never dialed.
    let server = BlossomServerUrl::parse("https://cdn.example.com").expect("server url");

    let err = prepared
        .upload(&client, &server, &auth_b)
        .await
        .expect_err("uploading the held bytes A under an authorization for B must be refused");
    assert_eq!(
        err,
        MediaUploadError::Blossom(UploadError::AuthorizationBlobMismatch {
            expected: hash_a,
            authorized_verb: BlossomVerb::Upload,
            authorized_blob: Some(hash_b),
        })
    );
}

/// Falsifier 3 (#559): a `MediaUploadError` value and a `MediaComposeError`
/// value cannot be conflated -- they are SEPARATE enums and separate return
/// types. There is deliberately no `From` impl between them, so no `?` can
/// merge an upload failure into a compose failure (or vice versa) silently;
/// the two wildcard-free matches below pin each domain's variants apart, and a
/// function in one domain cannot satisfy the other's return type.
#[test]
fn upload_failure_and_compose_failure_are_different_types() {
    fn upload_domain(err: MediaUploadError) -> MediaUploadError {
        err
    }
    fn compose_domain(err: MediaComposeError) -> MediaComposeError {
        err
    }

    let upload_err = upload_domain(MediaUploadError::Blossom(UploadError::Network {
        detail: "transport died".to_string(),
    }));
    let compose_err = compose_domain(MediaComposeError::NoImages);

    // Wildcard-free exhaustiveness over BOTH enums, in the same test, so the
    // two domains are demonstrably distinct sets of outcomes.
    match upload_err {
        MediaUploadError::Blossom(_) => {}
    }
    match compose_err {
        MediaComposeError::NoImages => {}
        MediaComposeError::Image(_) => {}
        MediaComposeError::Build(_) => {}
    }
}

/// Falsifier 4 (#559): compose_picture with identical inputs (same author,
/// same verified-upload-backed asset, same metadata, same FIXED created_at)
/// yields byte-identical `unsigned_event.as_json()` -- the Rust-side half of
/// the cross-surface parity contract. The seam introduces no nondeterminism,
/// and created_at is caller-supplied (never `now()`), so the composed body is
/// fully determined by its inputs. The direct/FFI `nmp-parity` oracle for the
/// seam lands with the later projection unit.
#[tokio::test]
async fn compose_is_deterministic() {
    let blob = b"deterministic compose image bytes";
    let now = Timestamp::now();
    let keys = Keys::generate();
    let author = keys.public_key();
    let asset = upload_one(&keys, blob, Some("image/png"), now).await;

    let post = PicturePost {
        title: Some("Same title".to_string()),
        description: "same description".to_string(),
        content_warning: Some(ContentWarning {
            reason: Some("nsfw".to_string()),
        }),
        hashtags: vec!["a".to_string(), "b".to_string()],
    };

    // A FIXED created_at (not `now()`): the parity/determinism contract is
    // that identical inputs yield a byte-identical unsigned body, which is
    // only well-defined once created_at is caller-supplied. This also makes
    // the falsifier non-flaky at second boundaries (#538 discipline).
    let composed_at = Timestamp::from(1_700_000_000u64);
    let first = compose_picture(
        author,
        composed_at,
        vec![ComposedImage::new(asset.clone())
            .with_dim(ImageDim {
                width: 8,
                height: 9,
            })
            .with_alt("alt text".to_string())],
        &post,
    )
    .expect("first compose");
    let second = compose_picture(
        author,
        composed_at,
        vec![ComposedImage::new(asset.clone())
            .with_dim(ImageDim {
                width: 8,
                height: 9,
            })
            .with_alt("alt text".to_string())],
        &post,
    )
    .expect("second compose");

    assert_eq!(first.as_json(), second.as_json());
}

/// Falsifier 5 (#559): prepare refuses an inverted authorization window
/// (`expiration <= created_at`) as `PrepareError::Authorization` wrapping the
/// upstream `AuthDraftError`, and refuses an empty mime as
/// `PrepareError::EmptyMimeType` -- both early, typed, and before any I/O.
#[test]
fn prepare_refuses_inverted_expiration_and_empty_mime() {
    let now = Timestamp::now();
    let author = Keys::generate().public_key();

    let created_at = Timestamp::from(now.as_secs());
    let inverted = prepare(
        b"image bytes".to_vec(),
        "image/png",
        author,
        created_at,
        created_at,
        "expired at birth",
    )
    .expect_err("an expiration at or before created_at must be refused");
    assert_eq!(
        inverted,
        PrepareError::Authorization(AuthDraftError::ExpirationNotAfterCreatedAt {
            created_at,
            expiration: created_at,
        })
    );

    let empty_mime = prepare(
        b"image bytes".to_vec(),
        "",
        author,
        past(now),
        future(now),
        "no mime",
    )
    .expect_err("an empty mime must be refused");
    assert_eq!(empty_mime, PrepareError::EmptyMimeType);
}

/// Falsifier 6 (#559): compose_picture with zero images is refused with
/// `MediaComposeError::NoImages` -- a kind:20 picture with no artifact is
/// unrepresentable (#421).
#[test]
fn compose_refuses_zero_images() {
    let author = Keys::generate().public_key();
    let post = PicturePost {
        title: None,
        description: "no pics".to_string(),
        content_warning: None,
        hashtags: vec![],
    };
    let err = compose_picture(author, Timestamp::from(1_700_000_000u64), vec![], &post)
        .expect_err("zero images refused");
    assert_eq!(err, MediaComposeError::NoImages);
}

/// Falsifier (#559): a verified asset whose server descriptor carried NO mime
/// type cannot mint a NIP-68 image -- compose surfaces it as the per-image
/// provenance failure `MediaComposeError::Image(MissingMimeType)`, never a
/// silently dropped image (#421). Constructs the `Image` variant.
#[tokio::test]
async fn a_mimeless_asset_fails_compose_as_image_provenance() {
    let blob = b"mimeless descriptor image bytes";
    let now = Timestamp::now();
    let keys = Keys::generate();
    let asset = upload_one(&keys, blob, None, now).await;

    let post = PicturePost {
        title: None,
        description: "x".to_string(),
        content_warning: None,
        hashtags: vec![],
    };
    let err = compose_picture(
        keys.public_key(),
        now,
        vec![ComposedImage::new(asset)],
        &post,
    )
    .expect_err("a mimeless asset must be refused");
    assert_eq!(
        err,
        MediaComposeError::Image(PictureImageError::MissingMimeType)
    );
}

/// Falsifier (#559): a kind:20 assembly failure (an empty hashtag) surfaces as
/// `MediaComposeError::Build`, distinct from a per-image provenance failure.
/// Constructs the `Build` variant.
#[tokio::test]
async fn an_empty_hashtag_surfaces_as_build_failure() {
    let blob = b"build-failure image bytes";
    let now = Timestamp::now();
    let keys = Keys::generate();
    let asset = upload_one(&keys, blob, Some("image/jpeg"), now).await;

    let post = PicturePost {
        title: None,
        description: "x".to_string(),
        content_warning: None,
        hashtags: vec!["ok".to_string(), String::new()],
    };
    let err = compose_picture(
        keys.public_key(),
        now,
        vec![ComposedImage::new(asset)],
        &post,
    )
    .expect_err("an empty hashtag must be refused");
    assert_eq!(
        err,
        MediaComposeError::Build(PictureBuildError::EmptyHashtag)
    );
}

/// Falsifier 7 (#559): wildcard-free exhaustiveness matches over all THREE
/// error enums, so adding a variant to any of the three separate failure
/// domains forces this test to be updated -- the taxonomies can never quietly
/// grow.
#[test]
fn error_taxonomies_are_wildcard_free_and_exhaustive() {
    fn _prepare(error: PrepareError) {
        match error {
            PrepareError::EmptyMimeType => {}
            PrepareError::Authorization(_) => {}
        }
    }
    fn _upload(error: MediaUploadError) {
        match error {
            MediaUploadError::Blossom(_) => {}
        }
    }
    fn _compose(error: MediaComposeError) {
        match error {
            MediaComposeError::NoImages => {}
            MediaComposeError::Image(_) => {}
            MediaComposeError::Build(_) => {}
        }
    }
}
