//! Witness-typed NIP-68 picture composition over UniFFI (#730, epic #216).
//!
//! This module projects the existing `nmp-media`/`nmp-nip68` seam without
//! inventing a second composer:
//!
//! ```text
//! FfiVerifiedUpload -> FfiComposedImage -> FfiPictureDraft
//! ```
//!
//! The upload witness is the exact opaque object returned by
//! [`crate::blossom::FfiBlossomClient`]. A plain/listed
//! [`crate::blossom::FfiBlobDescriptor`] cannot enter this path. The result is
//! an opaque immutable kind:20 draft: callers may request its existing
//! sign-only body or ordinary write intent, but cannot select another kind or
//! inject raw `imeta` rows through this operation.
//!
//! Failure domains remain separate. Blossom upload failures stay
//! `FfiBlossomUploadError`; this module exposes only composition failures;
//! signer failures and receipt truth continue through the existing
//! `NmpSignEventHandle` and `NmpReceiptStream` paths.

use std::sync::Arc;

use nmp_media::{
    compose_picture as compose_picture_direct, ComposedImage, MediaComposeError, PicturePost,
    UploadedAsset,
};
use nmp_nip68::{ContentWarning, ImageDim, PictureBuildError, PictureImageError};
use nostr::{JsonUtil, PublicKey, Timestamp, UnsignedEvent};

use crate::blossom::FfiVerifiedUpload;
use crate::types::{
    FfiDurability, FfiSignEventRequest, FfiWriteIntent, FfiWritePayload, FfiWriteRouting,
};

/// Pixel dimensions for one NIP-68 `imeta` image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct FfiImageDim {
    pub width: u32,
    pub height: u32,
}

/// One verified upload plus app-owned NIP-68 presentation metadata.
///
/// The mandatory `url`/`m`/`x` values are deliberately absent: they come only
/// from `upload`, whose opaque type has no public constructor. Optional
/// presentation strings are carried verbatim, matching the direct Rust API.
#[derive(Debug, Clone, uniffi::Record)]
pub struct FfiComposedImage {
    pub upload: Arc<FfiVerifiedUpload>,
    pub dim: Option<FfiImageDim>,
    pub alt: Option<String>,
    pub blurhash: Option<String>,
    pub thumbhash: Option<String>,
    pub fallbacks: Vec<String>,
}

/// Optional NIP-68 `content-warning` tag. `None` at the post field means no
/// tag; this record with `reason: None` means the one-cell tag.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiPictureContentWarning {
    pub reason: Option<String>,
}

/// Event-level metadata for a kind:20 picture draft.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiPicturePost {
    pub title: Option<String>,
    pub description: String,
    pub content_warning: Option<FfiPictureContentWarning>,
    pub hashtags: Vec<String>,
}

/// Composition-only failure taxonomy. Upload, signer, and publication
/// failures have their own existing types and never enter this enum.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum FfiPictureComposeError {
    /// Boundary-reintroduced parse refusal: direct Rust takes a proven key.
    InvalidAuthorPubkey { got: String },
    /// No verified images were supplied.
    NoImages,
    /// A verified descriptor omitted NIP-68's mandatory `m` value.
    ImageMissingMimeType,
    /// A supplied hashtag was empty.
    EmptyHashtag,
}

impl std::fmt::Display for FfiPictureComposeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidAuthorPubkey { got } => {
                write!(formatter, "invalid picture author public key: {got:?}")
            }
            Self::NoImages => {
                formatter.write_str("cannot compose a kind:20 picture without verified images")
            }
            Self::ImageMissingMimeType => formatter
                .write_str("verified image descriptor has no mime type required by NIP-68 imeta"),
            Self::EmptyHashtag => {
                formatter.write_str("cannot compose a kind:20 picture with an empty hashtag")
            }
        }
    }
}

impl std::error::Error for FfiPictureComposeError {}

fn compose_error_to_ffi(error: MediaComposeError) -> FfiPictureComposeError {
    match error {
        MediaComposeError::NoImages => FfiPictureComposeError::NoImages,
        MediaComposeError::Image(PictureImageError::MissingMimeType) => {
            FfiPictureComposeError::ImageMissingMimeType
        }
        MediaComposeError::Build(PictureBuildError::NoImages) => FfiPictureComposeError::NoImages,
        MediaComposeError::Build(PictureBuildError::EmptyHashtag) => {
            FfiPictureComposeError::EmptyHashtag
        }
    }
}

fn tags(event: &UnsignedEvent) -> Vec<Vec<String>> {
    event
        .tags
        .iter()
        .map(|tag| tag.as_slice().to_vec())
        .collect()
}

/// Immutable NIP-68 kind:20 draft. No constructor is exported: the only
/// creation site is [`compose_picture`], which consumes verified uploads and
/// the direct Rust composer. Its sign request and write intent are projections
/// of this same held event body.
#[derive(Debug, uniffi::Object)]
pub struct FfiPictureDraft {
    inner: UnsignedEvent,
}

#[uniffi::export]
impl FfiPictureDraft {
    pub fn author_pubkey_hex(&self) -> String {
        self.inner.pubkey.to_hex()
    }

    pub fn created_at(&self) -> u64 {
        self.inner.created_at.as_secs()
    }

    /// Always 20. Exposed for parity/inspection, never caller-selected.
    pub fn kind(&self) -> u16 {
        self.inner.kind.as_u16()
    }

    pub fn tags(&self) -> Vec<Vec<String>> {
        tags(&self.inner)
    }

    pub fn content(&self) -> String {
        self.inner.content.clone()
    }

    pub fn unsigned_event_json(&self) -> String {
        self.inner.as_json()
    }

    /// Existing governed sign-only input for this exact body. The request type
    /// deliberately omits its author, which the engine freezes from active
    /// identity; callers must keep that identity equal to
    /// `author_pubkey_hex`. The write-intent projection below carries the
    /// author explicitly and is the ordinary publication path.
    pub fn sign_request(&self) -> FfiSignEventRequest {
        FfiSignEventRequest {
            created_at: self.created_at(),
            kind: self.kind(),
            tags: self.tags(),
            content: self.content(),
        }
    }

    /// Existing ordinary write intent for this exact picture body. Routing and
    /// durability use the governed enums; kind/tags/content cannot be
    /// replaced through this typed operation.
    pub fn write_intent(
        &self,
        durability: FfiDurability,
        routing: FfiWriteRouting,
        identity_override: Option<String>,
        correlation: Option<String>,
    ) -> FfiWriteIntent {
        FfiWriteIntent {
            payload: FfiWritePayload::Unsigned {
                pubkey: self.author_pubkey_hex(),
                created_at: self.created_at(),
                kind: self.kind(),
                tags: self.tags(),
                content: self.content(),
            },
            durability,
            routing,
            identity_override,
            correlation,
        }
    }
}

/// Compose a kind:20 picture draft from verified uploads. There is no raw
/// descriptor, raw `imeta`, event-kind, or numeric routing input.
#[uniffi::export]
pub fn compose_picture(
    author_pubkey_hex: String,
    created_at: u64,
    images: Vec<FfiComposedImage>,
    post: FfiPicturePost,
) -> Result<Arc<FfiPictureDraft>, FfiPictureComposeError> {
    let author = PublicKey::parse(&author_pubkey_hex).map_err(|_| {
        FfiPictureComposeError::InvalidAuthorPubkey {
            got: author_pubkey_hex,
        }
    })?;

    let images = images
        .into_iter()
        .map(|image| {
            let asset = UploadedAsset::from_verified_upload(image.upload.inner.clone());
            let mut composed = ComposedImage::new(asset);
            if let Some(dim) = image.dim {
                composed = composed.with_dim(ImageDim {
                    width: dim.width,
                    height: dim.height,
                });
            }
            if let Some(alt) = image.alt {
                composed = composed.with_alt(alt);
            }
            if let Some(blurhash) = image.blurhash {
                composed = composed.with_blurhash(blurhash);
            }
            if let Some(thumbhash) = image.thumbhash {
                composed = composed.with_thumbhash(thumbhash);
            }
            for fallback in image.fallbacks {
                composed = composed.with_fallback(fallback);
            }
            composed
        })
        .collect();

    let post = PicturePost {
        title: post.title,
        description: post.description,
        content_warning: post.content_warning.map(|warning| ContentWarning {
            reason: warning.reason,
        }),
        hashtags: post.hashtags,
    };

    let inner = compose_picture_direct(author, Timestamp::from(created_at), images, &post)
        .map_err(compose_error_to_ffi)?;
    Ok(Arc::new(FfiPictureDraft { inner }))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread::JoinHandle;

    use nmp_blossom::Sha256Hash;
    use nostr::{JsonUtil, Keys, UnsignedEvent};
    use serde_json::Value;

    use super::*;
    use crate::blossom::{
        blossom_upload_authorization_draft, FfiBlossomAuthorization, FfiBlossomClient,
        FfiBlossomClientConfig, FfiBlossomUploadError, FfiBlossomVerb,
    };

    const FIXTURE: &str = include_str!("../../../fixtures/nip68-media-parity.json");

    struct MockServer {
        base_url: String,
        handle: JoinHandle<()>,
    }

    impl MockServer {
        fn descriptor(blob: &[u8], url: &str, mime_type: Option<&str>) -> Self {
            Self::descriptor_with_hash(blob, url, mime_type, None)
        }

        fn descriptor_with_hash(
            blob: &[u8],
            url: &str,
            mime_type: Option<&str>,
            returned_sha256_hex: Option<String>,
        ) -> Self {
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind mock server");
            let port = listener.local_addr().expect("mock address").port();
            let hash = returned_sha256_hex.unwrap_or_else(|| Sha256Hash::of(blob).to_hex());
            let mut body = format!(r#"{{"url":"{url}","sha256":"{hash}","size":{}"#, blob.len());
            if let Some(mime_type) = mime_type {
                body.push_str(&format!(r#","type":"{mime_type}""#));
            }
            body.push('}');

            let handle = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept mock connection");
                drain_http_request(&mut stream);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write mock response");
            });
            Self {
                base_url: format!("http://127.0.0.1:{port}"),
                handle,
            }
        }

        fn join(self) {
            self.handle.join().expect("mock server thread");
        }
    }

    fn drain_http_request(stream: &mut std::net::TcpStream) {
        let mut received = Vec::new();
        let mut buffer = [0u8; 4096];
        let header_end = loop {
            if let Some(position) = received.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                break position + 4;
            }
            let count = stream.read(&mut buffer).expect("read request headers");
            assert!(count > 0, "request ended before its headers");
            received.extend_from_slice(&buffer[..count]);
        };
        let headers = String::from_utf8_lossy(&received[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        let mut body_length = received.len() - header_end;
        while body_length < content_length {
            let count = stream.read(&mut buffer).expect("read request body");
            assert!(count > 0, "request ended before its body");
            body_length += count;
        }
    }

    fn fixture() -> Value {
        serde_json::from_str(FIXTURE).expect("shared fixture parses")
    }

    fn text<'a>(value: &'a Value, path: &[&str]) -> &'a str {
        let mut cursor = value;
        for key in path {
            cursor = &cursor[*key];
        }
        cursor.as_str().expect("fixture text")
    }

    fn number(value: &Value, path: &[&str]) -> u64 {
        let mut cursor = value;
        for key in path {
            cursor = &cursor[*key];
        }
        cursor.as_u64().expect("fixture number")
    }

    fn strings(value: &Value, path: &[&str]) -> Vec<String> {
        let mut cursor = value;
        for key in path {
            cursor = &cursor[*key];
        }
        cursor
            .as_array()
            .expect("fixture array")
            .iter()
            .map(|item| item.as_str().expect("fixture string").to_string())
            .collect()
    }

    fn authorization(keys: &Keys, blob: &[u8]) -> Arc<FfiBlossomAuthorization> {
        let now = Timestamp::now().as_secs();
        let hash = Sha256Hash::of(blob).to_hex();
        let draft = blossom_upload_authorization_draft(
            keys.public_key().to_hex(),
            hash.clone(),
            now - 5,
            now + 300,
            "NIP-68 parity upload".to_string(),
        )
        .expect("authorization draft");
        let unsigned =
            UnsignedEvent::from_json(&draft.unsigned_event_json).expect("draft JSON parses");
        let signed = unsigned.sign_with_keys(keys).expect("fixture signing");
        FfiBlossomAuthorization::validate(signed.as_json(), FfiBlossomVerb::Upload, Some(hash), now)
            .expect("fixture authorization validates")
    }

    fn upload_fixture(mime_type: Option<&str>) -> Arc<FfiVerifiedUpload> {
        let fixture = fixture();
        let blob = text(&fixture, &["blob_utf8"]).as_bytes();
        let keys = Keys::parse(text(&fixture, &["secret_key_hex"])).expect("fixture secret");
        let server =
            MockServer::descriptor(blob, text(&fixture, &["descriptor", "url"]), mime_type);
        let client = FfiBlossomClient::new(FfiBlossomClientConfig {
            allowed_local_hosts: vec!["127.0.0.1".to_string()],
            max_response_bytes: None,
            max_list_response_bytes: None,
            request_deadline_secs: Some(5),
        });
        let upload = client
            .upload(
                server.base_url.clone(),
                blob.to_vec(),
                mime_type.map(str::to_string),
                authorization(&keys, blob),
            )
            .expect("verified fixture upload");
        server.join();
        upload
    }

    fn ffi_image(upload: Arc<FfiVerifiedUpload>) -> FfiComposedImage {
        let fixture = fixture();
        FfiComposedImage {
            upload,
            dim: Some(FfiImageDim {
                width: number(&fixture, &["image", "width"]) as u32,
                height: number(&fixture, &["image", "height"]) as u32,
            }),
            alt: Some(text(&fixture, &["image", "alt"]).to_string()),
            blurhash: Some(text(&fixture, &["image", "blurhash"]).to_string()),
            thumbhash: Some(text(&fixture, &["image", "thumbhash"]).to_string()),
            fallbacks: strings(&fixture, &["image", "fallbacks"]),
        }
    }

    fn ffi_post() -> FfiPicturePost {
        let fixture = fixture();
        FfiPicturePost {
            title: Some(text(&fixture, &["post", "title"]).to_string()),
            description: text(&fixture, &["post", "description"]).to_string(),
            content_warning: Some(FfiPictureContentWarning {
                reason: Some(text(&fixture, &["post", "content_warning_reason"]).to_string()),
            }),
            hashtags: strings(&fixture, &["post", "hashtags"]),
        }
    }

    /// #730 shared parity falsifier: one real verified upload feeds both the
    /// direct Rust and FFI composers. The final unsigned event JSON is
    /// byte-identical, and the governed sign/write projections retain the
    /// exact kind/tags/content plus typed durability/routing.
    #[test]
    fn shared_fixture_is_byte_identical_across_direct_rust_and_ffi() {
        let fixture = fixture();
        let upload = upload_fixture(Some(text(&fixture, &["descriptor", "mime_type"])));
        let ffi_image = ffi_image(Arc::clone(&upload));
        let ffi_post = ffi_post();
        let author_hex = text(&fixture, &["author_pubkey_hex"]).to_string();
        let created_at = number(&fixture, &["created_at"]);

        let asset = UploadedAsset::from_verified_upload(upload.inner.clone());
        let image = ComposedImage::new(asset)
            .with_dim(ImageDim {
                width: number(&fixture, &["image", "width"]) as u32,
                height: number(&fixture, &["image", "height"]) as u32,
            })
            .with_alt(text(&fixture, &["image", "alt"]).to_string())
            .with_blurhash(text(&fixture, &["image", "blurhash"]).to_string())
            .with_thumbhash(text(&fixture, &["image", "thumbhash"]).to_string())
            .with_fallback(strings(&fixture, &["image", "fallbacks"])[0].clone())
            .with_fallback(strings(&fixture, &["image", "fallbacks"])[1].clone());
        let direct_post = PicturePost {
            title: Some(text(&fixture, &["post", "title"]).to_string()),
            description: text(&fixture, &["post", "description"]).to_string(),
            content_warning: Some(ContentWarning {
                reason: Some(text(&fixture, &["post", "content_warning_reason"]).to_string()),
            }),
            hashtags: strings(&fixture, &["post", "hashtags"]),
        };
        let direct = compose_picture_direct(
            PublicKey::parse(&author_hex).expect("fixture author"),
            Timestamp::from(created_at),
            vec![image],
            &direct_post,
        )
        .expect("direct compose");
        let ffi = compose_picture(author_hex.clone(), created_at, vec![ffi_image], ffi_post)
            .expect("FFI compose");

        assert_eq!(ffi.unsigned_event_json(), direct.as_json());
        assert_eq!(
            ffi.unsigned_event_json(),
            text(&fixture, &["expected", "unsigned_event_json"])
        );
        assert_eq!(
            direct.id.expect("composed event has computed id").to_hex(),
            text(&fixture, &["expected", "event_id"])
        );
        assert_eq!(ffi.kind(), number(&fixture, &["expected", "kind"]) as u16);
        let expected_tags: Vec<Vec<String>> =
            serde_json::from_value(fixture["expected"]["tags"].clone())
                .expect("expected fixture tags");
        assert_eq!(ffi.tags(), expected_tags);
        assert_eq!(
            ffi.content(),
            text(&fixture, &["expected", "content"]).to_string()
        );
        assert_eq!(ffi.sign_request().kind, 20);
        assert_eq!(ffi.sign_request().tags, expected_tags);

        let intent = ffi.write_intent(
            FfiDurability::Durable,
            FfiWriteRouting::AuthorOutbox,
            None,
            Some("nip68-parity".to_string()),
        );
        assert_eq!(intent.durability, FfiDurability::Durable);
        assert_eq!(intent.routing, FfiWriteRouting::AuthorOutbox);
        assert_eq!(intent.correlation.as_deref(), Some("nip68-parity"));
        match intent.payload {
            FfiWritePayload::Unsigned {
                pubkey,
                created_at: intent_at,
                kind,
                tags,
                content,
            } => {
                assert_eq!(pubkey, author_hex);
                assert_eq!(intent_at, created_at);
                assert_eq!(kind, 20);
                assert_eq!(tags, expected_tags);
                assert_eq!(content, text(&fixture, &["expected", "content"]));
            }
            FfiWritePayload::Signed { .. } => panic!("picture draft must stay unsigned"),
        }
    }

    /// Artifact-provenance falsifier: a validly-authorized upload whose 200
    /// descriptor names another hash fails before any `FfiVerifiedUpload`
    /// object exists, so there is no value that could enter `compose_picture`.
    #[test]
    fn tampered_upload_descriptor_cannot_mint_composition_witness() {
        let fixture = fixture();
        let blob = text(&fixture, &["blob_utf8"]).as_bytes();
        let keys = Keys::parse(text(&fixture, &["secret_key_hex"])).expect("fixture secret");
        let expected = Sha256Hash::of(blob).to_hex();
        let returned = Sha256Hash::of(b"tampered response").to_hex();
        let server = MockServer::descriptor_with_hash(
            blob,
            text(&fixture, &["descriptor", "url"]),
            Some(text(&fixture, &["descriptor", "mime_type"])),
            Some(returned.clone()),
        );
        let client = FfiBlossomClient::new(FfiBlossomClientConfig {
            allowed_local_hosts: vec!["127.0.0.1".to_string()],
            max_response_bytes: None,
            max_list_response_bytes: None,
            request_deadline_secs: Some(5),
        });
        let error = client
            .upload(
                server.base_url.clone(),
                blob.to_vec(),
                Some(text(&fixture, &["descriptor", "mime_type"]).to_string()),
                authorization(&keys, blob),
            )
            .expect_err("tampered descriptor cannot produce verified witness");
        server.join();
        assert_eq!(
            error,
            FfiBlossomUploadError::Sha256Mismatch {
                expected_sha256_hex: expected,
                returned_sha256_hex: returned,
            }
        );
    }

    /// Reachability Gate: every boundary composition refusal has a real
    /// construction site. None is documentation-only.
    #[test]
    fn every_picture_compose_error_variant_is_reachable() {
        let fixture = fixture();
        let post = ffi_post();
        assert_eq!(
            compose_picture("not-a-key".to_string(), 1, vec![], post.clone())
                .expect_err("bad author"),
            FfiPictureComposeError::InvalidAuthorPubkey {
                got: "not-a-key".to_string()
            }
        );
        let author = text(&fixture, &["author_pubkey_hex"]).to_string();
        assert_eq!(
            compose_picture(author.clone(), 1, vec![], post.clone()).expect_err("no images"),
            FfiPictureComposeError::NoImages
        );

        let missing_mime = upload_fixture(None);
        assert_eq!(
            compose_picture(
                author.clone(),
                1,
                vec![ffi_image(missing_mime)],
                post.clone()
            )
            .expect_err("missing mime"),
            FfiPictureComposeError::ImageMissingMimeType
        );

        let verified = upload_fixture(Some("image/jpeg"));
        let mut empty_hashtag_post = post;
        empty_hashtag_post.hashtags.push(String::new());
        assert_eq!(
            compose_picture(author, 1, vec![ffi_image(verified)], empty_hashtag_post)
                .expect_err("empty hashtag"),
            FfiPictureComposeError::EmptyHashtag
        );
    }
}
