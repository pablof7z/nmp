//! The NIP-68 image artifact-reference vocabulary (#558, epic #216
//! T15-B-NIP68-IMETA).
//!
//! [`PictureImage`] is a TYPE WITNESS: its fields are private and its only
//! constructors ([`PictureImage::from_descriptor`],
//! [`PictureImage::from_verified_upload`]) take their `url`/`sha256`/`m` from a
//! Blossom [`BlobDescriptor`] -- so every `PictureImage` carries the
//! content-addressed provenance NIP-68 imeta requires (`url`, `m`, `x`) BY
//! CONSTRUCTION. There is no public struct literal; you cannot mint a NIP-68
//! image artifact without the mandatory content-addressed fields (the #421
//! "protected kind without artifact provenance fails" contract).
//!
//! CRITICAL DOCTRINE (carried from the #545 review): `descriptor.url` is
//! SERVER-CONTROLLED, UNTRUSTED text. It is carried verbatim into the imeta
//! `url` field and never parsed, resolved, or trusted -- only `sha256` is
//! content-addressed provenance.

use nmp_blossom::{BlobDescriptor, Sha256Hash, VerifiedUpload};

/// An image's pixel dimensions, the imeta `dim` value. Wire form is exactly
/// `WIDTHxHEIGHT` (decimal, single lowercase `x`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageDim {
    pub width: u32,
    pub height: u32,
}

/// [`ImageDim::parse`]'s failure modes. Exhaustive; every variant is
/// constructed by the unit tests below (Reachability Gate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageDimError {
    /// No lowercase `x` separator at all -- e.g. `"3024"` or `"3024X4032"`
    /// (uppercase `X` is not a separator; NIP-68 mandates lowercase `x`).
    MissingSeparator { value: String },
    /// More than one `x` separator -- e.g. `"3024x40x32"`.
    MultipleSeparators { value: String },
    /// The width or height component is empty -- e.g. `"3024x"` or `"x4032"`.
    EmptyComponent { value: String },
    /// A component is not a decimal `u32` -- a non-`[0-9]` character (so `+`,
    /// whitespace, or hex letters are refused, never coerced) or an overflow.
    NotDecimal { component: String },
}

impl std::fmt::Display for ImageDimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingSeparator { value } => {
                write!(f, "imeta dim {value:?} has no lowercase `x` separator")
            }
            Self::MultipleSeparators { value } => {
                write!(f, "imeta dim {value:?} has more than one `x` separator")
            }
            Self::EmptyComponent { value } => {
                write!(f, "imeta dim {value:?} has an empty width or height")
            }
            Self::NotDecimal { component } => {
                write!(f, "imeta dim component {component:?} is not a decimal u32")
            }
        }
    }
}

impl std::error::Error for ImageDimError {}

impl ImageDim {
    /// The canonical `WIDTHxHEIGHT` wire form (decimal, lowercase `x`).
    pub fn to_wire(&self) -> String {
        format!("{}x{}", self.width, self.height)
    }

    /// STRICT parse of a `WIDTHxHEIGHT` value: exactly one lowercase `x`,
    /// both sides non-empty decimal `u32`. Uppercase `X`, a missing side, a
    /// second separator, or a non-decimal component is a typed refusal, never
    /// repaired.
    pub fn parse(value: &str) -> Result<Self, ImageDimError> {
        let parts: Vec<&str> = value.split('x').collect();
        match parts.len() {
            1 => {
                return Err(ImageDimError::MissingSeparator {
                    value: value.to_string(),
                })
            }
            2 => {}
            _ => {
                return Err(ImageDimError::MultipleSeparators {
                    value: value.to_string(),
                })
            }
        }
        let (width_text, height_text) = (parts[0], parts[1]);
        if width_text.is_empty() || height_text.is_empty() {
            return Err(ImageDimError::EmptyComponent {
                value: value.to_string(),
            });
        }
        let width = parse_decimal(width_text)?;
        let height = parse_decimal(height_text)?;
        Ok(Self { width, height })
    }
}

/// STRICT decimal `u32`: every byte must be an ASCII digit (so a leading `+`,
/// whitespace, or a hex letter is refused rather than coerced), and the value
/// must fit `u32`.
fn parse_decimal(component: &str) -> Result<u32, ImageDimError> {
    let is_decimal = component.bytes().all(|byte| byte.is_ascii_digit());
    if is_decimal {
        if let Ok(value) = component.parse::<u32>() {
            return Ok(value);
        }
    }
    Err(ImageDimError::NotDecimal {
        component: component.to_string(),
    })
}

/// A NIP-68 image artifact reference -- one imeta row's worth of picture
/// metadata. Private fields + provenance-only constructors: every value
/// carries `url`/`m`/`x` from a content-addressed Blossom descriptor by
/// construction (#421). Optionals are added through the builder-style setters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PictureImage {
    url: String,
    mime_type: String,
    sha256: Sha256Hash,
    dim: Option<ImageDim>,
    alt: Option<String>,
    blurhash: Option<String>,
    thumbhash: Option<String>,
    fallbacks: Vec<String>,
}

/// [`PictureImage::from_descriptor`]'s failure mode. Exhaustive; constructed
/// by the `a_descriptor_without_mime_cannot_mint_an_image` falsifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PictureImageError {
    /// The descriptor has no `mime_type` (`m`), which NIP-68 imeta REQUIRES.
    /// A NIP-68 image artifact cannot be minted without the mandatory
    /// content-addressed fields (`url`, `m`, `x`) -- #421.
    MissingMimeType,
}

impl std::fmt::Display for PictureImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingMimeType => f.write_str(
                "cannot mint a NIP-68 image: descriptor has no mime type, but imeta requires `m`",
            ),
        }
    }
}

impl std::error::Error for PictureImageError {}

impl PictureImage {
    /// Mint an image artifact from a content-addressed Blossom descriptor.
    /// `url`/`sha256` come from the descriptor; `mime_type` comes from
    /// `descriptor.mime_type`, and its absence is [`PictureImageError::
    /// MissingMimeType`] -- NO `PictureImage` is constructed (the
    /// protected-kind-without-provenance contract, #421). `descriptor.url` is
    /// carried VERBATIM as untrusted server text; only `sha256` is trusted.
    pub fn from_descriptor(descriptor: &BlobDescriptor) -> Result<Self, PictureImageError> {
        let mime_type = descriptor
            .mime_type
            .clone()
            .ok_or(PictureImageError::MissingMimeType)?;
        Ok(Self {
            url: descriptor.url.clone(),
            mime_type,
            sha256: descriptor.sha256,
            dim: None,
            alt: None,
            blurhash: None,
            thumbhash: None,
            fallbacks: Vec::new(),
        })
    }

    /// Mint an image artifact from a [`VerifiedUpload`] -- the descriptor whose
    /// `sha256` was PROVEN against the uploaded bytes. Delegates to
    /// [`Self::from_descriptor`] on the verified descriptor.
    pub fn from_verified_upload(upload: &VerifiedUpload) -> Result<Self, PictureImageError> {
        Self::from_descriptor(upload.descriptor())
    }

    /// Attach pixel dimensions (imeta `dim`).
    pub fn with_dim(mut self, dim: ImageDim) -> Self {
        self.dim = Some(dim);
        self
    }

    /// Attach alt text (imeta `alt`).
    pub fn with_alt(mut self, alt: String) -> Self {
        self.alt = Some(alt);
        self
    }

    /// Attach a blurhash (imeta `blurhash`).
    pub fn with_blurhash(mut self, blurhash: String) -> Self {
        self.blurhash = Some(blurhash);
        self
    }

    /// Attach a thumbhash (imeta `thumbhash`).
    pub fn with_thumbhash(mut self, thumbhash: String) -> Self {
        self.thumbhash = Some(thumbhash);
        self
    }

    /// Append a fallback url (imeta `fallback`, repeatable). Server-controlled,
    /// untrusted text -- carried verbatim, same as `url`.
    pub fn with_fallback(mut self, fallback: String) -> Self {
        self.fallbacks.push(fallback);
        self
    }

    /// The (untrusted, server-controlled) blob url.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The mime type (imeta `m`).
    pub fn mime_type(&self) -> &str {
        &self.mime_type
    }

    /// The content-addressed sha256 (imeta `x`) -- the ONLY trusted field.
    pub fn sha256(&self) -> Sha256Hash {
        self.sha256
    }

    /// The pixel dimensions, if set.
    pub fn dim(&self) -> Option<ImageDim> {
        self.dim
    }

    /// The alt text, if set.
    pub fn alt(&self) -> Option<&str> {
        self.alt.as_deref()
    }

    /// The blurhash, if set.
    pub fn blurhash(&self) -> Option<&str> {
        self.blurhash.as_deref()
    }

    /// The thumbhash, if set.
    pub fn thumbhash(&self) -> Option<&str> {
        self.thumbhash.as_deref()
    }

    /// The fallback urls, in append order.
    pub fn fallbacks(&self) -> &[String] {
        &self.fallbacks
    }

    /// The ordered imeta tag row for `Tag::parse`:
    /// `["imeta", "url ...", "m ...", "x ...", <optionals...>]`. Required
    /// provenance (`url`, `m`, `x`) is always first, in that order; optionals
    /// follow in a fixed order; `fallback` is emitted once per entry.
    pub fn imeta_tag_values(&self) -> Vec<String> {
        let mut values = vec![
            "imeta".to_string(),
            format!("url {}", self.url),
            format!("m {}", self.mime_type),
            format!("x {}", self.sha256.to_hex()),
        ];
        if let Some(dim) = self.dim {
            values.push(format!("dim {}", dim.to_wire()));
        }
        if let Some(alt) = &self.alt {
            values.push(format!("alt {alt}"));
        }
        if let Some(blurhash) = &self.blurhash {
            values.push(format!("blurhash {blurhash}"));
        }
        if let Some(thumbhash) = &self.thumbhash {
            values.push(format!("thumbhash {thumbhash}"));
        }
        for fallback in &self.fallbacks {
            values.push(format!("fallback {fallback}"));
        }
        values
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor_with_mime(mime: Option<&str>) -> BlobDescriptor {
        BlobDescriptor {
            url: "https://cdn.example.com/blob".to_string(),
            sha256: Sha256Hash::of(b"blob"),
            size: 4,
            mime_type: mime.map(str::to_string),
            uploaded: None,
        }
    }

    /// Invariant (#558): `dim` wire form is strict `WIDTHxHEIGHT` -- the
    /// `dim_wire_format_is_strict_widthxheight` falsifier. Each rejection
    /// constructs a distinct `ImageDimError` variant (Reachability Gate).
    #[test]
    fn dim_parse_is_strict_and_round_trips() {
        let dim = ImageDim::parse("3024x4032").expect("valid dim");
        assert_eq!(
            dim,
            ImageDim {
                width: 3024,
                height: 4032
            }
        );
        assert_eq!(dim.to_wire(), "3024x4032");

        assert_eq!(
            ImageDim::parse("3024"),
            Err(ImageDimError::MissingSeparator {
                value: "3024".to_string()
            })
        );
        // Uppercase `X` is not a separator -> no lowercase `x` at all.
        assert_eq!(
            ImageDim::parse("3024X4032"),
            Err(ImageDimError::MissingSeparator {
                value: "3024X4032".to_string()
            })
        );
        assert_eq!(
            ImageDim::parse("3024x40x32"),
            Err(ImageDimError::MultipleSeparators {
                value: "3024x40x32".to_string()
            })
        );
        assert_eq!(
            ImageDim::parse("3024x"),
            Err(ImageDimError::EmptyComponent {
                value: "3024x".to_string()
            })
        );
        assert_eq!(
            ImageDim::parse("x4032"),
            Err(ImageDimError::EmptyComponent {
                value: "x4032".to_string()
            })
        );
        assert_eq!(
            ImageDim::parse("30a4x4032"),
            Err(ImageDimError::NotDecimal {
                component: "30a4".to_string()
            })
        );
        // A `+`-prefixed component is refused, never coerced.
        assert!(matches!(
            ImageDim::parse("+30x40"),
            Err(ImageDimError::NotDecimal { .. })
        ));
    }

    /// Invariant (#558): `a_descriptor_without_mime_cannot_mint_an_image` --
    /// a descriptor with `mime_type: None` yields `MissingMimeType` and NO
    /// `PictureImage` (#421 protected-kind-without-provenance).
    #[test]
    fn a_descriptor_without_mime_cannot_mint_an_image() {
        assert_eq!(
            PictureImage::from_descriptor(&descriptor_with_mime(None)),
            Err(PictureImageError::MissingMimeType)
        );
    }

    /// Invariant (#558): a well-formed descriptor mints an image carrying the
    /// server url verbatim and the content-addressed sha256, and its imeta row
    /// leads with `url`/`m`/`x` in order.
    #[test]
    fn from_descriptor_binds_provenance_into_the_imeta_row() {
        let descriptor = descriptor_with_mime(Some("image/png"));
        let image = PictureImage::from_descriptor(&descriptor)
            .expect("descriptor with mime mints an image")
            .with_dim(ImageDim {
                width: 10,
                height: 20,
            })
            .with_alt("a cat".to_string())
            .with_fallback("https://mirror.example.com/blob".to_string());
        assert_eq!(image.url(), "https://cdn.example.com/blob");
        assert_eq!(image.mime_type(), "image/png");
        assert_eq!(image.sha256(), Sha256Hash::of(b"blob"));

        let row = image.imeta_tag_values();
        assert_eq!(row[0], "imeta");
        assert_eq!(row[1], "url https://cdn.example.com/blob");
        assert_eq!(row[2], "m image/png");
        assert_eq!(row[3], format!("x {}", Sha256Hash::of(b"blob").to_hex()));
        assert_eq!(row[4], "dim 10x20");
        assert_eq!(row[5], "alt a cat");
        assert_eq!(row[6], "fallback https://mirror.example.com/blob");
    }
}
