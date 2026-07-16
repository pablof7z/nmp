//! Build an immutable, unsigned NIP-68 kind:20 picture-first event draft
//! (#558, epic #216 T15-B-NIP68-IMETA).
//!
//! Following the `nmp-nip29`/`nmp-blossom` discipline: this builder NEVER
//! signs. It emits an [`nostr::UnsignedEvent`]; the caller signs it with the
//! existing `nmp-signer` machinery (signing and publishing are orthogonal
//! stages, #47/#32), and it never touches the engine.
//!
//! Every image is a [`PictureImage`] -- a type witness that carries `url`/`m`/
//! `x` content-addressed provenance by construction -- so a built kind:20
//! cannot carry an imeta without its mandatory fields. A spec with ZERO images
//! is refused ([`PictureBuildError::NoImages`]): a kind:20 picture with no
//! image artifact is unrepresentable (#421).

use nostr::{EventBuilder, Kind, PublicKey, Tag, UnsignedEvent};

use crate::image::PictureImage;

/// NIP-68 kind:20 event kind number.
pub const PICTURE_KIND: u16 = 20;

/// An optional `content-warning` tag: `["content-warning"]` or
/// `["content-warning", reason]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentWarning {
    /// The optional free-text reason.
    pub reason: Option<String>,
}

/// The full input to [`build_picture`]. `images` must be non-empty;
/// `description` is the event `content`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PictureSpec {
    /// The image artifacts, in order -- one imeta tag each. Non-empty.
    pub images: Vec<PictureImage>,
    /// Free-text description of the post (the event `content`).
    pub description: String,
    /// Optional short title (`["title", ...]`).
    pub title: Option<String>,
    /// Optional content warning.
    pub content_warning: Option<ContentWarning>,
    /// Optional `t` hashtags (one `["t", tag]` each). Empties are refused.
    pub hashtags: Vec<String>,
}

/// [`build_picture`]'s failure modes. Exhaustive; every variant is constructed
/// by a test (Reachability Gate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PictureBuildError {
    /// The spec had zero images -- a kind:20 picture with no image artifact is
    /// unrepresentable (#421 "protected kind without artifact provenance
    /// fails").
    NoImages,
    /// A hashtag was the empty string -- fail closed rather than emit an empty
    /// `["t", ""]` row.
    EmptyHashtag,
}

impl std::fmt::Display for PictureBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoImages => {
                f.write_str("cannot build a NIP-68 kind:20 picture with zero image artifacts")
            }
            Self::EmptyHashtag => {
                f.write_str("cannot build a NIP-68 kind:20 picture with an empty hashtag")
            }
        }
    }
}

impl std::error::Error for PictureBuildError {}

/// Build the unsigned kind:20 draft. Tag order: `title` (if present), then one
/// `imeta` per image (in `spec.images` order), then `content-warning` (if
/// present), then one `t` per hashtag. NEVER signs -- returns an
/// [`UnsignedEvent`] for the caller's signer.
pub fn build_picture(
    author: PublicKey,
    spec: &PictureSpec,
) -> Result<UnsignedEvent, PictureBuildError> {
    if spec.images.is_empty() {
        return Err(PictureBuildError::NoImages);
    }
    if spec.hashtags.iter().any(|hashtag| hashtag.is_empty()) {
        return Err(PictureBuildError::EmptyHashtag);
    }

    let mut tags: Vec<Tag> = Vec::new();

    if let Some(title) = &spec.title {
        tags.push(
            Tag::parse(["title", title.as_str()]).expect("a two-cell `title` row is never empty"),
        );
    }

    for image in &spec.images {
        // `imeta_tag_values` always leads with the literal "imeta", so the row
        // is never empty -- the only way `Tag::parse` can fail.
        tags.push(
            Tag::parse(image.imeta_tag_values()).expect("an imeta row always starts with `imeta`"),
        );
    }

    if let Some(content_warning) = &spec.content_warning {
        let mut row = vec!["content-warning".to_string()];
        if let Some(reason) = &content_warning.reason {
            row.push(reason.clone());
        }
        tags.push(Tag::parse(row).expect("a `content-warning` row is never empty"));
    }

    for hashtag in &spec.hashtags {
        tags.push(Tag::parse(["t", hashtag.as_str()]).expect("a two-cell `t` row is never empty"));
    }

    Ok(
        EventBuilder::new(Kind::from(PICTURE_KIND), spec.description.as_str())
            .tags(tags)
            .build(author),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::ImageDim;
    use nmp_blossom::{BlobDescriptor, Sha256Hash};

    fn author() -> PublicKey {
        nostr::Keys::generate().public_key()
    }

    fn image(seed: &[u8]) -> PictureImage {
        let descriptor = BlobDescriptor {
            url: format!("https://cdn.example.com/{}", Sha256Hash::of(seed).to_hex()),
            sha256: Sha256Hash::of(seed),
            size: seed.len() as u64,
            mime_type: Some("image/jpeg".to_string()),
            uploaded: None,
        };
        PictureImage::from_descriptor(&descriptor).expect("descriptor with mime")
    }

    fn tag_name(tag: &Tag) -> &str {
        tag.as_slice().first().map(String::as_str).unwrap_or("")
    }

    /// Invariant (#558): `a_picture_with_no_images_is_refused` -- an empty
    /// `images` vec yields `NoImages` (#421).
    #[test]
    fn a_picture_with_no_images_is_refused() {
        let spec = PictureSpec {
            images: vec![],
            description: "no pics".to_string(),
            title: None,
            content_warning: None,
            hashtags: vec![],
        };
        assert_eq!(
            build_picture(author(), &spec),
            Err(PictureBuildError::NoImages)
        );
    }

    /// Invariant (#558): `empty_hashtag_is_refused` -- an empty `t` string is a
    /// typed refusal, never an empty `["t", ""]` row.
    #[test]
    fn empty_hashtag_is_refused() {
        let spec = PictureSpec {
            images: vec![image(b"a")],
            description: "hi".to_string(),
            title: None,
            content_warning: None,
            hashtags: vec!["ok".to_string(), String::new()],
        };
        assert_eq!(
            build_picture(author(), &spec),
            Err(PictureBuildError::EmptyHashtag)
        );
    }

    /// Invariant (#558): tag order is title, then one imeta per image in order,
    /// then content-warning, then t tags -- and each image's imeta carries its
    /// own `x`.
    #[test]
    fn tag_order_and_per_image_imeta() {
        let spec = PictureSpec {
            images: vec![
                image(b"first").with_dim(ImageDim {
                    width: 1,
                    height: 2,
                }),
                image(b"second"),
            ],
            description: "two pics".to_string(),
            title: Some("A title".to_string()),
            content_warning: Some(ContentWarning {
                reason: Some("nsfw".to_string()),
            }),
            hashtags: vec!["cats".to_string()],
        };
        let event = build_picture(author(), &spec).expect("valid spec builds");
        assert_eq!(event.kind, Kind::from(PICTURE_KIND));
        assert_eq!(event.content, "two pics");

        let names: Vec<&str> = event.tags.iter().map(tag_name).collect();
        assert_eq!(
            names,
            vec!["title", "imeta", "imeta", "content-warning", "t"]
        );

        // First imeta carries the first image's sha256, second the second's.
        let imetas: Vec<&Tag> = event
            .tags
            .iter()
            .filter(|tag| tag_name(tag) == "imeta")
            .collect();
        assert!(imetas[0]
            .as_slice()
            .iter()
            .any(|value| value == &format!("x {}", Sha256Hash::of(b"first").to_hex())));
        assert!(imetas[1]
            .as_slice()
            .iter()
            .any(|value| value == &format!("x {}", Sha256Hash::of(b"second").to_hex())));
    }

    /// A `content-warning` with no reason emits a single-cell row.
    #[test]
    fn content_warning_without_reason_is_single_cell() {
        let spec = PictureSpec {
            images: vec![image(b"a")],
            description: "hi".to_string(),
            title: None,
            content_warning: Some(ContentWarning { reason: None }),
            hashtags: vec![],
        };
        let event = build_picture(author(), &spec).expect("valid");
        let cw = event
            .tags
            .iter()
            .find(|tag| tag_name(tag) == "content-warning")
            .expect("content-warning present");
        assert_eq!(cw.as_slice(), &["content-warning".to_string()]);
    }
}
