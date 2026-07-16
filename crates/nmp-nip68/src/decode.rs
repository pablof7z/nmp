//! Decode a NIP-68 kind:20 event into typed picture facts (#558, epic #216
//! T15-B-NIP68-IMETA). The `nmp-content::article` read-model pattern: a
//! tolerant decoder over both a public [`nostr::Event`] and a raw
//! `(id, author, created_at, tags, content)` tuple.
//!
//! DECODE IS TOLERANT and NEVER PANICS. It is the "determine artifact claim
//! from event shape" read side: an imeta lacking `x` decodes into a
//! [`DecodedImage`] with `sha256: None` PLUS an
//! [`PictureDiagnostic::ImetaMissingSha256`], so a consumer (T15-C, or a
//! protected-kind guard) can SEE the artifact has no provenance rather than
//! silently trusting it. Unknown imeta keys are recorded, not rejected.
//!
//! Server-controlled fields (`url`, `fallback`) are carried VERBATIM: the
//! decoder neither validates nor sanitizes them. Only `x` (sha256) is parsed
//! through the strict content-addressed codec.

use nostr::Event;

use nmp_blossom::Sha256Hash;

use crate::build::ContentWarning;
use crate::image::ImageDim;

/// A decoded imeta row -- the PUBLIC read model mirroring `PictureImage`, but
/// every provenance field is `Option` because the wire MAY omit it. A
/// `DecodedImage` with `sha256: None` is an UNPROVENANCED artifact.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecodedImage {
    /// The (untrusted, server-controlled) url, verbatim -- `None` if absent.
    pub url: Option<String>,
    /// The mime type (`m`) -- `None` if absent.
    pub mime_type: Option<String>,
    /// The content-addressed sha256 (`x`) -- `None` if absent OR unparsable
    /// (an `ImetaBadSha256`/`ImetaMissingSha256` diagnostic distinguishes the
    /// two).
    pub sha256: Option<Sha256Hash>,
    /// The pixel dimensions (`dim`) -- `None` if absent or unparsable.
    pub dim: Option<ImageDim>,
    /// The alt text (`alt`).
    pub alt: Option<String>,
    /// The blurhash (`blurhash`).
    pub blurhash: Option<String>,
    /// The thumbhash (`thumbhash`).
    pub thumbhash: Option<String>,
    /// The fallback urls (`fallback`), verbatim, in order.
    pub fallbacks: Vec<String>,
}

/// A tolerant-decode observation. Exhaustive; every variant is constructed by a
/// test (Reachability Gate). Diagnostics are RECORDED, never a hard error -- a
/// consumer inspects them to decide how much of a decoded picture to trust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PictureDiagnostic {
    /// The imeta at `index` had no `url`.
    ImetaMissingUrl { index: usize },
    /// The imeta at `index` had no `m` (mime).
    ImetaMissingMime { index: usize },
    /// The imeta at `index` had no `x` (sha256) -- unprovenanced artifact.
    ImetaMissingSha256 { index: usize },
    /// The imeta at `index` had an `x` that failed the strict sha256 codec.
    ImetaBadSha256 { index: usize },
    /// The imeta at `index` had a `dim` that failed strict `WIDTHxHEIGHT`.
    ImetaBadDim { index: usize, value: String },
    /// The event carried no imeta tag at all.
    NoImages,
    /// The imeta at `index` carried a key this crate does not model -- tolerated
    /// and recorded, not rejected.
    UnknownImetaKey { index: usize, key: String },
}

impl std::fmt::Display for PictureDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ImetaMissingUrl { index } => write!(f, "imeta #{index} is missing `url`"),
            Self::ImetaMissingMime { index } => write!(f, "imeta #{index} is missing `m`"),
            Self::ImetaMissingSha256 { index } => {
                write!(
                    f,
                    "imeta #{index} is missing `x` (no content-addressed provenance)"
                )
            }
            Self::ImetaBadSha256 { index } => write!(f, "imeta #{index} has an invalid `x` sha256"),
            Self::ImetaBadDim { index, value } => {
                write!(f, "imeta #{index} has an invalid `dim` {value:?}")
            }
            Self::NoImages => f.write_str("kind:20 event carries no imeta image"),
            Self::UnknownImetaKey { index, key } => {
                write!(f, "imeta #{index} carries unknown key {key:?}")
            }
        }
    }
}

impl std::error::Error for PictureDiagnostic {}

/// The typed decode of a NIP-68 kind:20 event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Picture {
    /// The event id (hex).
    pub event_id: String,
    /// The author pubkey (hex).
    pub author: String,
    /// The event `created_at` (unix seconds).
    pub created_at: u64,
    /// The `title` tag, if present.
    pub title: Option<String>,
    /// The event `content` (free-text description).
    pub description: String,
    /// One decoded image per imeta tag, in order.
    pub images: Vec<DecodedImage>,
    /// The `content-warning` tag, if present.
    pub content_warning: Option<ContentWarning>,
    /// The `t` hashtags, in order.
    pub hashtags: Vec<String>,
    /// Tolerant-decode observations (see [`PictureDiagnostic`]).
    pub diagnostics: Vec<PictureDiagnostic>,
}

/// Decode a public [`nostr::Event`] (delegates to [`decode_picture_from_raw`]).
pub fn decode_picture(event: &Event) -> Picture {
    decode_picture_from_raw(
        &event.id.to_hex(),
        &event.pubkey.to_hex(),
        event.created_at.as_secs(),
        event.tags.iter().map(|tag| tag.as_slice()),
        &event.content,
    )
}

/// Decode from raw fields -- the signing-free path (no `Keys` needed).
pub fn decode_picture_from_raw<'a>(
    event_id: &str,
    author: &str,
    created_at: u64,
    tags: impl IntoIterator<Item = &'a [String]>,
    content: &str,
) -> Picture {
    let mut picture = Picture {
        event_id: event_id.to_string(),
        author: author.to_string(),
        created_at,
        description: content.to_string(),
        ..Picture::default()
    };

    let mut imeta_index = 0usize;
    for tag in tags {
        let Some(name) = tag.first().map(String::as_str) else {
            continue;
        };
        match name {
            "title" if picture.title.is_none() => {
                picture.title = nonempty(tag.get(1).map(String::as_str).unwrap_or(""));
            }
            "imeta" => {
                let index = imeta_index;
                imeta_index += 1;
                let (image, mut diagnostics) = decode_imeta(index, &tag[1..]);
                picture.images.push(image);
                picture.diagnostics.append(&mut diagnostics);
            }
            "content-warning" if picture.content_warning.is_none() => {
                picture.content_warning = Some(ContentWarning {
                    reason: nonempty(tag.get(1).map(String::as_str).unwrap_or("")),
                });
            }
            "t" => {
                if let Some(hashtag) = nonempty(tag.get(1).map(String::as_str).unwrap_or("")) {
                    picture.hashtags.push(hashtag);
                }
            }
            _ => {}
        }
    }

    if picture.images.is_empty() {
        picture.diagnostics.push(PictureDiagnostic::NoImages);
    }

    picture
}

/// Decode one imeta tag's space-joined `key value` entries (the cells after the
/// leading "imeta"). Tolerant: unknown keys and unparsable values become
/// diagnostics, never panics.
fn decode_imeta(index: usize, entries: &[String]) -> (DecodedImage, Vec<PictureDiagnostic>) {
    let mut image = DecodedImage::default();
    let mut diagnostics = Vec::new();
    let mut saw_x = false;

    for entry in entries {
        // NIP-92/94 form: each entry is `"key value"`, space-separated at the
        // FIRST space -- the value keeps any further spaces verbatim.
        let (key, value) = match entry.split_once(' ') {
            Some((key, value)) => (key, value),
            None => (entry.as_str(), ""),
        };
        match key {
            // Server-controlled; carried verbatim, never interpreted.
            "url" => image.url = nonempty(value),
            "m" => image.mime_type = nonempty(value),
            "x" => {
                saw_x = true;
                match Sha256Hash::from_hex(value) {
                    Ok(hash) => image.sha256 = Some(hash),
                    Err(_) => diagnostics.push(PictureDiagnostic::ImetaBadSha256 { index }),
                }
            }
            "dim" => match ImageDim::parse(value) {
                Ok(dim) => image.dim = Some(dim),
                Err(_) => diagnostics.push(PictureDiagnostic::ImetaBadDim {
                    index,
                    value: value.to_string(),
                }),
            },
            "alt" => image.alt = nonempty(value),
            "blurhash" => image.blurhash = nonempty(value),
            "thumbhash" => image.thumbhash = nonempty(value),
            // Server-controlled; carried verbatim, repeatable.
            "fallback" => {
                if let Some(fallback) = nonempty(value) {
                    image.fallbacks.push(fallback);
                }
            }
            other => diagnostics.push(PictureDiagnostic::UnknownImetaKey {
                index,
                key: other.to_string(),
            }),
        }
    }

    if image.url.is_none() {
        diagnostics.push(PictureDiagnostic::ImetaMissingUrl { index });
    }
    if image.mime_type.is_none() {
        diagnostics.push(PictureDiagnostic::ImetaMissingMime { index });
    }
    if !saw_x {
        diagnostics.push(PictureDiagnostic::ImetaMissingSha256 { index });
    }

    (image, diagnostics)
}

/// Carry a value verbatim (no trimming -- the "never interpret server text"
/// doctrine), collapsing only the truly empty string to `None`.
fn nonempty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows_to_slices(rows: &[Vec<String>]) -> impl Iterator<Item = &[String]> {
        rows.iter().map(Vec::as_slice)
    }

    fn row(cells: &[&str]) -> Vec<String> {
        cells.iter().map(|c| c.to_string()).collect()
    }

    /// Invariant (#558): `decode_surfaces_missing_provenance_as_a_diagnostic_not_a_trust`
    /// -- an imeta with no `x` decodes to `sha256: None` PLUS
    /// `ImetaMissingSha256` (fail-closed read).
    #[test]
    fn decode_surfaces_missing_provenance_as_a_diagnostic_not_a_trust() {
        let rows = vec![row(&[
            "imeta",
            "url https://cdn.example.com/x",
            "m image/png",
        ])];
        let picture = decode_picture_from_raw("id", "author", 1, rows_to_slices(&rows), "desc");
        assert_eq!(picture.images.len(), 1);
        assert_eq!(picture.images[0].sha256, None);
        assert!(picture
            .diagnostics
            .contains(&PictureDiagnostic::ImetaMissingSha256 { index: 0 }));
    }

    /// A bad `x` records `ImetaBadSha256` (and NOT `ImetaMissingSha256`, since
    /// the key was present); a bad `dim` records `ImetaBadDim`; an unknown key
    /// is tolerated as `UnknownImetaKey`.
    #[test]
    fn bad_and_unknown_imeta_fields_are_recorded_not_fatal() {
        let rows = vec![row(&[
            "imeta",
            "url https://cdn.example.com/x",
            "m image/png",
            "x NOT-HEX",
            "dim 10",
            "weird value",
        ])];
        let picture = decode_picture_from_raw("id", "author", 1, rows_to_slices(&rows), "desc");
        let diagnostics = &picture.diagnostics;
        assert!(diagnostics.contains(&PictureDiagnostic::ImetaBadSha256 { index: 0 }));
        assert!(!diagnostics.contains(&PictureDiagnostic::ImetaMissingSha256 { index: 0 }));
        assert!(diagnostics.contains(&PictureDiagnostic::ImetaBadDim {
            index: 0,
            value: "10".to_string()
        }));
        assert!(diagnostics.contains(&PictureDiagnostic::UnknownImetaKey {
            index: 0,
            key: "weird".to_string()
        }));
    }

    /// An imeta missing `url` and `m` records both diagnostics.
    #[test]
    fn missing_url_and_mime_are_recorded() {
        let hex = Sha256Hash::of(b"blob").to_hex();
        let rows = vec![row(&["imeta", &format!("x {hex}")])];
        let picture = decode_picture_from_raw("id", "author", 1, rows_to_slices(&rows), "desc");
        assert!(picture
            .diagnostics
            .contains(&PictureDiagnostic::ImetaMissingUrl { index: 0 }));
        assert!(picture
            .diagnostics
            .contains(&PictureDiagnostic::ImetaMissingMime { index: 0 }));
    }

    /// An event with no imeta records `NoImages`.
    #[test]
    fn no_imeta_records_no_images() {
        let rows = vec![row(&["title", "just a title"])];
        let picture = decode_picture_from_raw("id", "author", 1, rows_to_slices(&rows), "desc");
        assert!(picture.images.is_empty());
        assert!(picture.diagnostics.contains(&PictureDiagnostic::NoImages));
    }

    /// Title, content-warning and t tags decode; content-warning reason is
    /// optional.
    #[test]
    fn event_level_tags_decode() {
        let hex = Sha256Hash::of(b"blob").to_hex();
        let rows = vec![
            row(&["title", "My photos"]),
            row(&["imeta", "url u", "m image/png", &format!("x {hex}")]),
            row(&["content-warning", "nsfw"]),
            row(&["t", "cats"]),
            row(&["t", "dogs"]),
        ];
        let picture = decode_picture_from_raw("id", "author", 42, rows_to_slices(&rows), "desc");
        assert_eq!(picture.title.as_deref(), Some("My photos"));
        assert_eq!(picture.created_at, 42);
        assert_eq!(
            picture.content_warning,
            Some(ContentWarning {
                reason: Some("nsfw".to_string())
            })
        );
        assert_eq!(
            picture.hashtags,
            vec!["cats".to_string(), "dogs".to_string()]
        );
    }

    /// Wildcard-free exhaustiveness witness over `PictureDiagnostic` -- a new
    /// variant added without a decode path breaks this match at compile time.
    #[test]
    fn picture_diagnostic_is_exhaustive() {
        let variants = [
            PictureDiagnostic::ImetaMissingUrl { index: 0 },
            PictureDiagnostic::ImetaMissingMime { index: 0 },
            PictureDiagnostic::ImetaMissingSha256 { index: 0 },
            PictureDiagnostic::ImetaBadSha256 { index: 0 },
            PictureDiagnostic::ImetaBadDim {
                index: 0,
                value: String::new(),
            },
            PictureDiagnostic::NoImages,
            PictureDiagnostic::UnknownImetaKey {
                index: 0,
                key: String::new(),
            },
        ];
        for variant in &variants {
            // Exhaustive match, no wildcard arm: adding a variant forces an
            // update here, and every variant has a non-empty Display.
            let described = match variant {
                PictureDiagnostic::ImetaMissingUrl { .. }
                | PictureDiagnostic::ImetaMissingMime { .. }
                | PictureDiagnostic::ImetaMissingSha256 { .. }
                | PictureDiagnostic::ImetaBadSha256 { .. }
                | PictureDiagnostic::ImetaBadDim { .. }
                | PictureDiagnostic::NoImages
                | PictureDiagnostic::UnknownImetaKey { .. } => variant.to_string(),
            };
            assert!(!described.is_empty());
        }
    }
}
