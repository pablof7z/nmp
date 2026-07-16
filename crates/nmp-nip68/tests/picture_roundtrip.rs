//! Integration falsifiers for the NIP-68 build -> decode round trip (#558,
//! epic #216 T15-B-NIP68-IMETA). Each test names the invariant it would
//! falsify.

use nmp_blossom::{BlobDescriptor, Sha256Hash};
use nmp_nip68::{
    build_picture, decode_picture, ContentWarning, ImageDim, PictureBuildError, PictureDiagnostic,
    PictureImage, PictureImageError, PictureSpec, PICTURE_KIND,
};
use nostr::{Keys, Kind, Timestamp};

fn descriptor(url: &str, seed: &[u8], mime: Option<&str>) -> BlobDescriptor {
    BlobDescriptor {
        url: url.to_string(),
        sha256: Sha256Hash::of(seed),
        size: seed.len() as u64,
        mime_type: mime.map(str::to_string),
        uploaded: None,
    }
}

/// A built kind:20 must be a valid signable draft: signing it with a real key
/// yields an event that decodes back to the same picture facts. This proves the
/// unsigned draft is well-formed without the crate ever signing.
fn sign_and_decode(spec: &PictureSpec, keys: &Keys) -> nmp_nip68::Picture {
    let unsigned = build_picture(keys.public_key(), Timestamp::from(1_700_000_000u64), spec)
        .expect("valid spec builds");
    let event = unsigned
        .sign_with_keys(keys)
        .expect("caller signs the draft");
    assert_eq!(event.kind, Kind::from(PICTURE_KIND));
    decode_picture(&event)
}

/// Falsifier 1 (#558): `build_picture_binds_every_image_sha256_into_its_imeta`.
/// decode(build(spec)) round-trips: each imeta carries `x` == the descriptor's
/// sha256, `url` == descriptor.url verbatim, `m` == mime; multi-image spec
/// yields one imeta per image, IN ORDER.
#[test]
fn build_picture_binds_every_image_sha256_into_its_imeta() {
    let keys = Keys::generate();
    let first = PictureImage::from_descriptor(&descriptor(
        "https://cdn.example.com/one",
        b"one",
        Some("image/png"),
    ))
    .expect("mime present")
    .with_dim(ImageDim {
        width: 3024,
        height: 4032,
    })
    .with_alt("first".to_string());
    let second = PictureImage::from_descriptor(&descriptor(
        "https://cdn.example.com/two",
        b"two",
        Some("image/jpeg"),
    ))
    .expect("mime present");

    let spec = PictureSpec {
        images: vec![first, second],
        description: "my album".to_string(),
        title: Some("Album".to_string()),
        content_warning: None,
        hashtags: vec![],
    };

    let picture = sign_and_decode(&spec, &keys);
    assert!(
        picture.diagnostics.is_empty(),
        "clean round trip: {:?}",
        picture.diagnostics
    );
    assert_eq!(picture.description, "my album");
    assert_eq!(picture.title.as_deref(), Some("Album"));
    assert_eq!(picture.images.len(), 2);

    assert_eq!(
        picture.images[0].url.as_deref(),
        Some("https://cdn.example.com/one")
    );
    assert_eq!(picture.images[0].mime_type.as_deref(), Some("image/png"));
    assert_eq!(picture.images[0].sha256, Some(Sha256Hash::of(b"one")));
    assert_eq!(
        picture.images[0].dim,
        Some(ImageDim {
            width: 3024,
            height: 4032
        })
    );
    assert_eq!(picture.images[0].alt.as_deref(), Some("first"));

    assert_eq!(
        picture.images[1].url.as_deref(),
        Some("https://cdn.example.com/two")
    );
    assert_eq!(picture.images[1].mime_type.as_deref(), Some("image/jpeg"));
    assert_eq!(picture.images[1].sha256, Some(Sha256Hash::of(b"two")));
}

/// Falsifier 6 (#558): `server_controlled_url_is_carried_verbatim_never_interpreted`.
/// A descriptor url with odd-but-valid characters is placed in imeta `url`
/// byte-for-byte and decoded back identically -- we neither validate nor
/// sanitize the server url; only sha256 is trusted.
#[test]
fn server_controlled_url_is_carried_verbatim_never_interpreted() {
    let keys = Keys::generate();
    let odd_url = "https://cdn.example.com/blob?token=a~b%20c&x=1!;(v=2)";
    let image = PictureImage::from_descriptor(&descriptor(odd_url, b"blob", Some("image/webp")))
        .expect("mime present");
    let spec = PictureSpec {
        images: vec![image],
        description: "verbatim".to_string(),
        title: None,
        content_warning: None,
        hashtags: vec![],
    };
    let picture = sign_and_decode(&spec, &keys);
    assert_eq!(picture.images[0].url.as_deref(), Some(odd_url));
}

/// Falsifier 6b (#558): the imeta "key value" space-join must survive VALUES
/// that themselves contain literal spaces -- decode splits each entry at the
/// FIRST space only, so multi-word `alt` text (the common case) and a url or
/// fallback carrying a real 0x20 byte round-trip byte-for-byte rather than
/// being truncated at the first internal space. This locks the
/// `split_once(' ')` contract the whole verbatim-carry doctrine rests on.
#[test]
fn imeta_values_containing_real_spaces_round_trip_intact() {
    let keys = Keys::generate();
    let spaced_url = "https://cdn.example.com/a photo.jpg";
    let alt = "a red bicycle leaning by the sea at dawn";
    let fallback = "https://mirror.example.com/a photo.jpg";
    let image =
        PictureImage::from_descriptor(&descriptor(spaced_url, b"spaced", Some("image/jpeg")))
            .expect("mime present")
            .with_alt(alt.to_string())
            .with_fallback(fallback.to_string());
    let spec = PictureSpec {
        images: vec![image],
        description: "spaces".to_string(),
        title: None,
        content_warning: None,
        hashtags: vec![],
    };
    let picture = sign_and_decode(&spec, &keys);
    assert_eq!(picture.images[0].url.as_deref(), Some(spaced_url));
    assert_eq!(picture.images[0].alt.as_deref(), Some(alt));
    assert_eq!(picture.images[0].fallbacks, vec![fallback.to_string()]);
    // No spurious "unknown key" diagnostic from a mid-value space being
    // misread as a new key/value boundary.
    assert!(
        picture.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        picture.diagnostics
    );
}

/// Falsifier 3 (#558): `a_picture_with_no_images_is_refused`.
#[test]
fn a_picture_with_no_images_is_refused() {
    let keys = Keys::generate();
    let spec = PictureSpec {
        images: vec![],
        description: "empty".to_string(),
        title: None,
        content_warning: None,
        hashtags: vec![],
    };
    assert_eq!(
        build_picture(keys.public_key(), Timestamp::from(1_700_000_000u64), &spec),
        Err(PictureBuildError::NoImages)
    );
}

/// Falsifier 2 (#558): `a_descriptor_without_mime_cannot_mint_an_image`.
#[test]
fn a_descriptor_without_mime_cannot_mint_an_image() {
    let no_mime = descriptor("https://cdn.example.com/x", b"x", None);
    assert_eq!(
        PictureImage::from_descriptor(&no_mime),
        Err(PictureImageError::MissingMimeType)
    );
}

/// Falsifier 7 (#558): `content_warning_round_trips` and `empty_hashtag_is_refused`.
#[test]
fn content_warning_round_trips_and_empty_hashtag_is_refused() {
    let keys = Keys::generate();
    let image = PictureImage::from_descriptor(&descriptor(
        "https://cdn.example.com/x",
        b"x",
        Some("image/png"),
    ))
    .expect("mime present");

    let spec = PictureSpec {
        images: vec![image.clone()],
        description: "cw".to_string(),
        title: None,
        content_warning: Some(ContentWarning {
            reason: Some("sensitive".to_string()),
        }),
        hashtags: vec!["nostr".to_string()],
    };
    let picture = sign_and_decode(&spec, &keys);
    assert_eq!(
        picture.content_warning,
        Some(ContentWarning {
            reason: Some("sensitive".to_string())
        })
    );
    assert_eq!(picture.hashtags, vec!["nostr".to_string()]);

    let bad = PictureSpec {
        images: vec![image],
        description: "cw".to_string(),
        title: None,
        content_warning: None,
        hashtags: vec![String::new()],
    };
    assert_eq!(
        build_picture(keys.public_key(), Timestamp::from(1_700_000_000u64), &bad),
        Err(PictureBuildError::EmptyHashtag)
    );
}

/// Falsifier 4 (#558): `decode_surfaces_missing_provenance_as_a_diagnostic_not_a_trust`
/// at the integration level -- a signed kind:20 whose imeta omits `x` decodes
/// to `sha256: None` + `ImetaMissingSha256`.
#[test]
fn decode_surfaces_missing_provenance_as_a_diagnostic_not_a_trust() {
    let keys = Keys::generate();
    // Build a real signed kind:20 by hand (no `x` in the imeta), proving the
    // tolerant decoder sees the missing provenance rather than trusting it.
    let event = nostr::EventBuilder::new(Kind::from(PICTURE_KIND), "no provenance")
        .tags([
            nostr::Tag::parse(["imeta", "url https://cdn.example.com/x", "m image/png"])
                .expect("imeta row"),
        ])
        .sign_with_keys(&keys)
        .expect("sign");
    let picture = decode_picture(&event);
    assert_eq!(picture.images.len(), 1);
    assert_eq!(picture.images[0].sha256, None);
    assert!(picture
        .diagnostics
        .contains(&PictureDiagnostic::ImetaMissingSha256 { index: 0 }));
}
