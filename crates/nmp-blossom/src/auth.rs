//! BUD-11 kind:24242 authorization events (#545, #551): draft
//! construction, fail-closed validation, and the HTTP header encoding.
//! This crate NEVER signs -- the draft builders return an
//! [`nostr::UnsignedEvent`] and the caller signs it with the existing
//! `nmp-signer` machinery (signing and publishing are orthogonal stages,
//! #47/#32). [`SignedAuthorization`] is the ONLY way to obtain an
//! `Authorization` header value: the type witnesses that every BUD-11
//! check passed (type-over-convention).

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use nostr::{
    Alphabet, Event, EventBuilder, JsonUtil, Kind, PublicKey, Tag, TagKind, Timestamp,
    UnsignedEvent,
};

use crate::sha256::Sha256Hash;

/// The BUD-11 authorization verbs, modeled TOTALLY so the verb-string
/// mapping can never drift per call site. Upload, delete, and list have
/// draft builders in this crate ([`upload_authorization_draft`],
/// [`delete_authorization_draft`], [`list_authorization_draft`]); `get`
/// has no builder yet -- it lands with the follow-up units under epic
/// #216. BUD-04 mirror deliberately has NO builder of its own: the spec
/// authorizes a mirror with the `upload` verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlossomVerb {
    Upload,
    Delete,
    Get,
    List,
}

impl BlossomVerb {
    /// The exact `t` tag value BUD-11 assigns this verb.
    pub fn as_tag_value(&self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::Delete => "delete",
            Self::Get => "get",
            Self::List => "list",
        }
    }

    /// The inverse of [`Self::as_tag_value`]. `None` for any string that is
    /// not exactly one of the four spec verbs (case-sensitive: BUD-11 tag
    /// values are lowercase).
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "upload" => Some(Self::Upload),
            "delete" => Some(Self::Delete),
            "get" => Some(Self::Get),
            "list" => Some(Self::List),
            _ => None,
        }
    }
}

impl std::fmt::Display for BlossomVerb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_tag_value())
    }
}

/// The draft builders' shared failure modes ([`upload_authorization_draft`],
/// [`delete_authorization_draft`], [`list_authorization_draft`]).
/// Exhaustive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthDraftError {
    /// BUD-11 requires `created_at` in the past and `expiration` in the
    /// future, so an expiration at or before `created_at` can never be a
    /// valid authorization window.
    ExpirationNotAfterCreatedAt {
        created_at: Timestamp,
        expiration: Timestamp,
    },
}

impl std::fmt::Display for AuthDraftError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExpirationNotAfterCreatedAt {
                created_at,
                expiration,
            } => write!(
                f,
                "authorization expiration {} is not after created_at {}",
                expiration.as_secs(),
                created_at.as_secs()
            ),
        }
    }
}

impl std::error::Error for AuthDraftError {}

/// The shared BUD-11 draft assembly all verb builders delegate to: content
/// = human-readable `description`, `["t", <verb>]`, an `["x", <lowercase
/// hex sha256>]` tag when the verb binds a blob, and `["expiration",
/// <unix ts>]`, in exactly that tag order (the `upload` builder's
/// observable output predates this factoring and must not change).
///
/// Refuses `expiration <= created_at` (typed): such a window is expired at
/// birth. Whether `created_at` is actually in the past and `expiration`
/// actually in the future is checked against a real clock at validation
/// time ([`SignedAuthorization::validate`]) -- draft construction has no
/// clock on purpose (deterministic composition, caller-supplied time).
fn authorization_draft(
    author: PublicKey,
    verb: BlossomVerb,
    blob: Option<Sha256Hash>,
    created_at: Timestamp,
    expiration: Timestamp,
    description: &str,
) -> Result<UnsignedEvent, AuthDraftError> {
    if expiration <= created_at {
        return Err(AuthDraftError::ExpirationNotAfterCreatedAt {
            created_at,
            expiration,
        });
    }
    let mut builder =
        EventBuilder::new(Kind::BlossomAuth, description).tag(Tag::hashtag(verb.as_tag_value()));
    if let Some(blob) = blob {
        builder = builder.tag(Tag::custom(
            TagKind::single_letter(Alphabet::X, false),
            [blob.to_hex()],
        ));
    }
    Ok(builder
        .tag(Tag::expiration(expiration))
        .custom_created_at(created_at)
        .build(author))
}

/// Compose an UNSIGNED BUD-11 `upload` authorization (kind 24242): content
/// = human-readable `description`, `["t","upload"]`, `["x", <lowercase hex
/// sha256 of the exact blob bytes>]`, `["expiration", <unix ts>]`. The
/// caller signs the returned draft with its own signer -- this crate never
/// holds keys. Refuses `expiration <= created_at` (typed); see
/// [`authorization_draft`] for the clock discipline.
///
/// BUD-04 NOTE (#551): a `PUT /mirror` request is authorized with THIS
/// builder -- the spec assigns mirroring the `upload` verb, with the `x`
/// tag bound to the mirrored blob's sha256. There is deliberately no
/// separate mirror builder.
pub fn upload_authorization_draft(
    author: PublicKey,
    blob: Sha256Hash,
    created_at: Timestamp,
    expiration: Timestamp,
    description: &str,
) -> Result<UnsignedEvent, AuthDraftError> {
    authorization_draft(
        author,
        BlossomVerb::Upload,
        Some(blob),
        created_at,
        expiration,
        description,
    )
}

/// Compose an UNSIGNED BUD-12 `delete` authorization (kind 24242):
/// `["t","delete"]`, `["x", <lowercase hex sha256 of the blob to
/// delete>]`, `["expiration", <unix ts>]`. Exactly ONE blob is bound:
/// BUD-12 mandates that multiple `x` tags MUST NOT be read as a request to
/// delete multiple blobs, so this builder never emits more than one.
/// Refuses `expiration <= created_at` (typed); see [`authorization_draft`]
/// for the clock discipline. The caller signs the returned draft -- this
/// crate never holds keys.
pub fn delete_authorization_draft(
    author: PublicKey,
    blob: Sha256Hash,
    created_at: Timestamp,
    expiration: Timestamp,
    description: &str,
) -> Result<UnsignedEvent, AuthDraftError> {
    authorization_draft(
        author,
        BlossomVerb::Delete,
        Some(blob),
        created_at,
        expiration,
        description,
    )
}

/// Compose an UNSIGNED BUD-12 `list` authorization (kind 24242):
/// `["t","list"]` and `["expiration", <unix ts>]`, NO `x` tag -- listing
/// is scoped to a pubkey by the request path, not to any blob (BUD-12).
/// Refuses `expiration <= created_at` (typed); see [`authorization_draft`]
/// for the clock discipline. The caller signs the returned draft -- this
/// crate never holds keys.
pub fn list_authorization_draft(
    author: PublicKey,
    created_at: Timestamp,
    expiration: Timestamp,
    description: &str,
) -> Result<UnsignedEvent, AuthDraftError> {
    authorization_draft(
        author,
        BlossomVerb::List,
        None,
        created_at,
        expiration,
        description,
    )
}

/// What a caller is about to use an authorization FOR. `blob` is `Some`
/// for verbs that bind a concrete blob (upload/delete); validation then
/// requires that exact hash among the event's `x` tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpectedAuthorization {
    pub verb: BlossomVerb,
    pub blob: Option<Sha256Hash>,
}

/// [`SignedAuthorization::validate`]'s failure modes. Exhaustive, and each
/// check is its own variant so a refused authorization names the exact
/// BUD-11 clause it failed. Every variant is pinned by
/// `tests/upload_contract.rs` falsifier 1 and the unit tests below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthValidationError {
    /// Not kind 24242.
    WrongKind { found: u16 },
    /// The Schnorr signature (or event id) does not verify.
    BadSignature { reason: String },
    /// No `t` tag with a value at all.
    MissingVerb,
    /// More than one `t` tag: a bearer token must grant exactly one verb,
    /// so a second (unvalidated) verb tag is refused outright.
    MultipleVerbs,
    /// The sole `t` tag names a different verb than expected (or a string
    /// that is not a BUD-11 verb).
    VerbMismatch {
        expected: BlossomVerb,
        found: String,
    },
    /// The expectation binds a blob but no `x` tag carries exactly that
    /// lowercase-hex sha256 (covers both "no x tags" and "none match";
    /// malformed x values cannot match because they fail the strict
    /// lowercase parser).
    BlobNotBound { expected: Sha256Hash },
    /// No `expiration` tag.
    MissingExpiration,
    /// The `expiration` tag is not in the future.
    Expired {
        expiration: Timestamp,
        now: Timestamp,
    },
    /// BUD-11 requires `created_at` in the past.
    CreatedAtInFuture {
        created_at: Timestamp,
        now: Timestamp,
    },
}

impl std::fmt::Display for AuthValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongKind { found } => {
                write!(f, "authorization event kind {found} is not 24242")
            }
            Self::BadSignature { reason } => {
                write!(f, "authorization event signature is invalid: {reason}")
            }
            Self::MissingVerb => f.write_str("authorization event has no `t` verb tag"),
            Self::MultipleVerbs => {
                f.write_str("authorization event carries more than one `t` verb tag")
            }
            Self::VerbMismatch { expected, found } => write!(
                f,
                "authorization verb {found:?} does not match expected {expected}"
            ),
            Self::BlobNotBound { expected } => write!(
                f,
                "authorization binds no `x` tag equal to {}",
                expected.to_hex()
            ),
            Self::MissingExpiration => f.write_str("authorization event has no `expiration` tag"),
            Self::Expired { expiration, now } => write!(
                f,
                "authorization expired: expiration {} is not after now {}",
                expiration.as_secs(),
                now.as_secs()
            ),
            Self::CreatedAtInFuture { created_at, now } => write!(
                f,
                "authorization created_at {} is after now {}",
                created_at.as_secs(),
                now.as_secs()
            ),
        }
    }
}

impl std::error::Error for AuthValidationError {}

/// A signed kind:24242 event PROVEN (at construction) to satisfy every
/// BUD-11 check against a concrete [`ExpectedAuthorization`] and clock.
/// The private field makes this the only door to
/// [`SignedAuthorization::header_value`]: an unvalidated event cannot be
/// encoded into an `Authorization` header (type-over-convention).
#[derive(Debug, Clone)]
pub struct SignedAuthorization {
    event: Event,
    verb: BlossomVerb,
    blob: Option<Sha256Hash>,
}

impl SignedAuthorization {
    /// Fail-closed BUD-11 validation. Check order (each failure its own
    /// typed variant): kind, signature, `t` verb, `x` blob binding (when
    /// `expected.blob` is `Some`; multiple `x` tags are allowed by spec but
    /// the expected hash MUST be among them), `expiration` presence,
    /// `expiration > now`, `created_at <= now`.
    pub fn validate(
        event: Event,
        expected: &ExpectedAuthorization,
        now: Timestamp,
    ) -> Result<Self, AuthValidationError> {
        if event.kind != Kind::BlossomAuth {
            return Err(AuthValidationError::WrongKind {
                found: event.kind.as_u16(),
            });
        }
        event
            .verify()
            .map_err(|error| AuthValidationError::BadSignature {
                reason: error.to_string(),
            })?;
        let (verb_value, extra_verb) = {
            let mut verb_tags = event
                .tags
                .filter(TagKind::t())
                .filter_map(|tag| tag.content());
            let first = verb_tags
                .next()
                .ok_or(AuthValidationError::MissingVerb)?
                .to_string();
            (first, verb_tags.next().is_some())
        };
        // Fail closed on more than one `t` tag: the header is a bearer
        // token, and a second verb would ride to the server unvalidated
        // (adversarial-review finding, #545). Multiple `x` tags are
        // spec-sanctioned; multiple verbs are not.
        if extra_verb {
            return Err(AuthValidationError::MultipleVerbs);
        }
        if BlossomVerb::parse(&verb_value) != Some(expected.verb) {
            return Err(AuthValidationError::VerbMismatch {
                expected: expected.verb,
                found: verb_value,
            });
        }
        if let Some(expected_blob) = expected.blob {
            let bound = event
                .tags
                .filter(TagKind::single_letter(Alphabet::X, false))
                .filter_map(|tag| tag.content())
                .filter_map(|value| Sha256Hash::from_hex(value).ok())
                .any(|hash| hash == expected_blob);
            if !bound {
                return Err(AuthValidationError::BlobNotBound {
                    expected: expected_blob,
                });
            }
        }
        let expiration = *event
            .tags
            .expiration()
            .ok_or(AuthValidationError::MissingExpiration)?;
        if expiration <= now {
            return Err(AuthValidationError::Expired { expiration, now });
        }
        if event.created_at > now {
            return Err(AuthValidationError::CreatedAtInFuture {
                created_at: event.created_at,
                now,
            });
        }
        Ok(Self {
            event,
            verb: expected.verb,
            blob: expected.blob,
        })
    }

    /// The BUD-11 HTTP transmission form:
    /// `Nostr <base64url-no-padding(event canonical JSON)>`.
    pub fn header_value(&self) -> String {
        format!("Nostr {}", URL_SAFE_NO_PAD.encode(self.event.as_json()))
    }

    /// The verb this authorization was validated FOR.
    pub fn verb(&self) -> BlossomVerb {
        self.verb
    }

    /// The blob hash this authorization was proven to bind (`None` for
    /// verbs validated without a blob binding).
    pub fn blob(&self) -> Option<Sha256Hash> {
        self.blob
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Invariant (#545): the verb enum's tag-value mapping is total and
    /// round-trips exactly; unknown or wrongly-cased strings parse to None.
    #[test]
    fn verb_tag_values_round_trip_totally() {
        for verb in [
            BlossomVerb::Upload,
            BlossomVerb::Delete,
            BlossomVerb::Get,
            BlossomVerb::List,
        ] {
            assert_eq!(BlossomVerb::parse(verb.as_tag_value()), Some(verb));
        }
        assert_eq!(BlossomVerb::parse("Upload"), None);
        assert_eq!(BlossomVerb::parse("mirror"), None);
        assert_eq!(BlossomVerb::parse(""), None);
    }

    /// Invariant (#545): a validated authorization missing its `t` tag
    /// entirely is `MissingVerb`, distinct from a mismatched verb.
    #[test]
    fn missing_verb_tag_is_distinct_from_a_mismatched_one() {
        let keys = nostr::Keys::generate();
        let blob = Sha256Hash::of(b"blob");
        let now = Timestamp::from(1_700_000_000u64);
        let unsigned = EventBuilder::new(Kind::BlossomAuth, "no verb")
            .tag(Tag::custom(
                TagKind::single_letter(Alphabet::X, false),
                [blob.to_hex()],
            ))
            .tag(Tag::expiration(Timestamp::from(now.as_secs() + 600)))
            .custom_created_at(Timestamp::from(now.as_secs() - 5))
            .build(keys.public_key());
        let event = unsigned.sign_with_keys(&keys).expect("test signing");
        let expected = ExpectedAuthorization {
            verb: BlossomVerb::Upload,
            blob: Some(blob),
        };
        let err = SignedAuthorization::validate(event, &expected, now)
            .expect_err("a verbless authorization must be refused");
        assert_eq!(err, AuthValidationError::MissingVerb);
    }

    /// Invariant (#545, adversarial-review finding): a bearer token grants
    /// exactly one verb -- an event carrying `["t","upload"]` AND
    /// `["t","delete"]` is refused as `MultipleVerbs`, never validated on
    /// its first tag while the second rides to the server unvalidated.
    #[test]
    fn a_second_verb_tag_is_refused_outright() {
        let keys = nostr::Keys::generate();
        let blob = Sha256Hash::of(b"blob");
        let now = Timestamp::from(1_700_000_000u64);
        let unsigned = EventBuilder::new(Kind::BlossomAuth, "two verbs")
            .tag(Tag::hashtag("upload"))
            .tag(Tag::hashtag("delete"))
            .tag(Tag::custom(
                TagKind::single_letter(Alphabet::X, false),
                [blob.to_hex()],
            ))
            .tag(Tag::expiration(Timestamp::from(now.as_secs() + 600)))
            .custom_created_at(Timestamp::from(now.as_secs() - 5))
            .build(keys.public_key());
        let event = unsigned.sign_with_keys(&keys).expect("test signing");
        let expected = ExpectedAuthorization {
            verb: BlossomVerb::Upload,
            blob: Some(blob),
        };
        let err = SignedAuthorization::validate(event, &expected, now)
            .expect_err("a two-verb authorization must be refused");
        assert_eq!(err, AuthValidationError::MultipleVerbs);
    }

    /// Invariant (#545, adversarial-review pin): a malformed (non-hex) `x`
    /// tag alongside the correct binding neither panics nor rejects -- the
    /// garbage tag grants nothing and the exact hash still binds.
    #[test]
    fn garbage_x_tag_alongside_the_correct_binding_still_validates() {
        let keys = nostr::Keys::generate();
        let blob = Sha256Hash::of(b"blob");
        let now = Timestamp::from(1_700_000_000u64);
        let unsigned = EventBuilder::new(Kind::BlossomAuth, "upload with noise")
            .tag(Tag::hashtag("upload"))
            .tag(Tag::custom(
                TagKind::single_letter(Alphabet::X, false),
                ["NOT-A-HASH".to_string()],
            ))
            .tag(Tag::custom(
                TagKind::single_letter(Alphabet::X, false),
                [blob.to_hex()],
            ))
            .tag(Tag::expiration(Timestamp::from(now.as_secs() + 600)))
            .custom_created_at(Timestamp::from(now.as_secs() - 5))
            .build(keys.public_key());
        let event = unsigned.sign_with_keys(&keys).expect("test signing");
        let expected = ExpectedAuthorization {
            verb: BlossomVerb::Upload,
            blob: Some(blob),
        };
        let auth = SignedAuthorization::validate(event, &expected, now)
            .expect("the exact hash binds despite adjacent garbage");
        assert_eq!(auth.blob(), Some(blob));
    }

    /// Invariant (#551): the delete builder emits kind 24242 with
    /// `["t","delete"]`, EXACTLY ONE `x` tag (BUD-12 forbids reading
    /// multiple `x` tags as a multi-blob delete, so the builder can never
    /// emit more), and the expiration; the list builder emits
    /// `["t","list"]` with NO `x` tag at all; and both route through the
    /// same expired-at-birth refusal as the upload builder.
    #[test]
    fn delete_and_list_drafts_carry_the_spec_tag_shapes() {
        let keys = nostr::Keys::generate();
        let blob = Sha256Hash::of(b"blob");
        let created_at = Timestamp::from(1_700_000_000u64);
        let expiration = Timestamp::from(created_at.as_secs() + 600);

        let delete_draft = delete_authorization_draft(
            keys.public_key(),
            blob,
            created_at,
            expiration,
            "delete a blob",
        )
        .expect("a future expiration");
        assert_eq!(delete_draft.kind, Kind::BlossomAuth);
        assert_eq!(delete_draft.created_at, created_at);
        assert_eq!(
            delete_draft
                .tags
                .find(TagKind::t())
                .and_then(|tag| tag.content()),
            Some("delete")
        );
        let x_values: Vec<String> = delete_draft
            .tags
            .filter(TagKind::single_letter(Alphabet::X, false))
            .filter_map(|tag| tag.content())
            .map(str::to_string)
            .collect();
        assert_eq!(x_values, vec![blob.to_hex()]);
        assert_eq!(delete_draft.tags.expiration(), Some(&expiration));

        let list_draft =
            list_authorization_draft(keys.public_key(), created_at, expiration, "list my blobs")
                .expect("a future expiration");
        assert_eq!(list_draft.kind, Kind::BlossomAuth);
        assert_eq!(
            list_draft
                .tags
                .find(TagKind::t())
                .and_then(|tag| tag.content()),
            Some("list")
        );
        assert_eq!(
            list_draft
                .tags
                .filter(TagKind::single_letter(Alphabet::X, false))
                .count(),
            0,
            "a list authorization binds no blob"
        );
        assert_eq!(list_draft.tags.expiration(), Some(&expiration));

        // Both new builders refuse an expiration at/before created_at,
        // through the same shared gate as the upload builder.
        assert_eq!(
            delete_authorization_draft(
                keys.public_key(),
                blob,
                created_at,
                created_at,
                "delete a blob",
            ),
            Err(AuthDraftError::ExpirationNotAfterCreatedAt {
                created_at,
                expiration: created_at,
            })
        );
        assert_eq!(
            list_authorization_draft(keys.public_key(), created_at, created_at, "list my blobs"),
            Err(AuthDraftError::ExpirationNotAfterCreatedAt {
                created_at,
                expiration: created_at,
            })
        );
    }

    /// Invariant (#545): a wrong-kind event is refused before any other
    /// check -- an ordinary note can never become an Authorization header.
    #[test]
    fn wrong_kind_event_is_refused() {
        let keys = nostr::Keys::generate();
        let blob = Sha256Hash::of(b"blob");
        let now = Timestamp::from(1_700_000_000u64);
        let unsigned = EventBuilder::new(Kind::from(1u16), "a note")
            .custom_created_at(Timestamp::from(now.as_secs() - 5))
            .build(keys.public_key());
        let event = unsigned.sign_with_keys(&keys).expect("test signing");
        let expected = ExpectedAuthorization {
            verb: BlossomVerb::Upload,
            blob: Some(blob),
        };
        let err = SignedAuthorization::validate(event, &expected, now)
            .expect_err("a wrong-kind event must be refused");
        assert_eq!(err, AuthValidationError::WrongKind { found: 1 });
    }

    /// Invariant (#545): a future created_at is refused (BUD-11 mandates
    /// created_at in the past).
    #[test]
    fn future_created_at_is_refused() {
        let keys = nostr::Keys::generate();
        let blob = Sha256Hash::of(b"blob");
        let now = Timestamp::from(1_700_000_000u64);
        let created_at = Timestamp::from(now.as_secs() + 100);
        let unsigned = upload_authorization_draft(
            keys.public_key(),
            blob,
            created_at,
            Timestamp::from(now.as_secs() + 600),
            "upload a blob",
        )
        .expect("expiration after created_at");
        let event = unsigned.sign_with_keys(&keys).expect("test signing");
        let expected = ExpectedAuthorization {
            verb: BlossomVerb::Upload,
            blob: Some(blob),
        };
        let err = SignedAuthorization::validate(event, &expected, now)
            .expect_err("a future created_at must be refused");
        assert_eq!(
            err,
            AuthValidationError::CreatedAtInFuture { created_at, now }
        );
    }
}
