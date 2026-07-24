//! Blossom (BUD-01/02/03/04/11/12) blob projection over UniFFI (#555/#731,
//! epic
//! #216 T15-A-BLOSSOM): `nmp-blossom`'s draft builders, fail-closed
//! authorization validation, and the self-verifying blob client, exported
//! in the same engine-less shape as [`crate::entity`]/[`crate::nip29`] --
//! none of this needs an `NmpEngine` instance, and `nmp-blossom` stays an
//! opt-in protocol crate bundled here only because `nmp-ffi` is the ONE
//! staticlib/cdylib every Swift/Kotlin app links (the `nmp-nip29`
//! precedent, see `Cargo.toml`).
//!
//! SEPARATED TAXONOMIES CROSS WHOLE: every operation keeps its own error
//! enum ([`FfiBlossomUploadError`], [`FfiBlossomMirrorError`],
//! [`FfiBlossomDeleteError`], [`FfiBlossomListError`],
//! [`FfiBlossomAuthError`]) mirroring the Rust taxonomy variant-for-variant
//! -- a deliberate departure from widening the shared
//! [`crate::convert::FfiError`], because `nmp-blossom`'s own design rule is
//! that blob-operation failures are never collapsed across operations, and
//! funneling four exhaustive taxonomies into the engine-wide enum would
//! collapse exactly what the crate separates.
//!
//! FALLIBILITY RE-INTRODUCED AT THE BOUNDARY (the [`crate::nip29`]
//! documented pattern): server URLs, pubkeys, and sha256 digests cross as
//! raw strings, so their parses fail HERE with typed variants
//! (`InvalidServerUrl`/`InvalidOwnerPubkey`/`InvalidBlobSha256`/...) even
//! though the direct-Rust API takes already-proven types.
//!
//! SIGNING: this module NEVER signs, exactly like the crate underneath.
//! The draft builders return the unsigned kind:24242 event BOTH as
//! canonical JSON (for native/external signers such as NIP-55) AND as the
//! raw `created_at`/`kind`/`tags`/`content` tokens that feed straight into
//! the engine's governed sign-only operation
//! (`NmpEngine::sign_event(FfiSignEventRequest, ...)`, whose author is
//! frozen from the active account -- so the draft's `author_pubkey_hex`
//! must be that account for the engine path). The signed result
//! ([`crate::types::FfiSignedEvent`]) then enters
//! [`FfiBlossomAuthorization::validate_signed_event`]; a raw event JSON
//! string enters [`FfiBlossomAuthorization::validate`]. Both doors run the
//! full BUD-11 fail-closed validation -- an unvalidated event can never
//! become an `Authorization` header on this side of the boundary either.
//!
//! BLOCKING CLIENT DISCIPLINE: [`FfiBlossomClient`]'s methods BLOCK the
//! calling thread for up to the configured request deadline -- native
//! wrappers dispatch them off the main thread (Swift: continuation +
//! global queue; Kotlin: `Dispatchers.IO`). Each call builds a fresh
//! current-thread tokio runtime AND a fresh `BlossomClient` inside it,
//! mirroring `nmp-engine/src/relay_information.rs`'s `http_runtime()`
//! discipline: the reqwest/hickory stack is born and dropped inside the
//! flight's own runtime, so hickory cannot retain DNS state bound to a
//! runtime that no longer exists on the next call.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use nmp_blossom::{
    AuthDraftError, AuthValidationError, BlobDescriptor, BlossomClient, BlossomClientConfig,
    BlossomServerUrl, BlossomVerb, DeleteError, DescriptorError, ExpectedAuthorization, ListError,
    ListPage, MalformedServerEntryReason, MirrorError, ServerAdmission, ServerAdmissionRefusal,
    ServerCandidatePolicy, ServerCandidateSource, ServerUrlError, Sha256Hash, Sha256HexError,
    SignedAuthorization, UploadError, DEFAULT_MAX_LIST_RESPONSE_BYTES, DEFAULT_MAX_RESPONSE_BYTES,
    DEFAULT_REQUEST_DEADLINE,
};
use nostr::{JsonUtil, PublicKey, Timestamp, UnsignedEvent};

use crate::convert::demand_to_ffi;
use crate::types::{FfiDemand, FfiRow, FfiSignedEvent};

/// The BUD-11 authorization verbs (`nmp_blossom::BlossomVerb` mirror).
/// `get` is modeled totally (the Rust enum is total) but has no draft
/// builder yet -- the `get`/`media` endpoints are epic-#216 follow-ups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiBlossomVerb {
    Upload,
    Delete,
    Get,
    List,
}

fn verb_to_ffi(verb: BlossomVerb) -> FfiBlossomVerb {
    match verb {
        BlossomVerb::Upload => FfiBlossomVerb::Upload,
        BlossomVerb::Delete => FfiBlossomVerb::Delete,
        BlossomVerb::Get => FfiBlossomVerb::Get,
        BlossomVerb::List => FfiBlossomVerb::List,
    }
}

fn verb_from_ffi(verb: FfiBlossomVerb) -> BlossomVerb {
    match verb {
        FfiBlossomVerb::Upload => BlossomVerb::Upload,
        FfiBlossomVerb::Delete => BlossomVerb::Delete,
        FfiBlossomVerb::Get => BlossomVerb::Get,
        FfiBlossomVerb::List => BlossomVerb::List,
    }
}

/// A BUD-02 blob descriptor (`nmp_blossom::BlobDescriptor` mirror). When
/// returned by [`FfiBlossomClient::upload`]/[`FfiBlossomClient::mirror`]
/// its `sha256` was PROVEN equal to the locally computed/authorized hash
/// (the `VerifiedUpload` integrity gate); rows from
/// [`FfiBlossomClient::list`] are strictly parsed but remain unverified
/// server claims, exactly as in the Rust crate.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiBlobDescriptor {
    pub url: String,
    /// 64 lowercase hex characters -- the strict BUD-01 blob identity.
    pub sha256: String,
    pub size: u64,
    pub mime_type: Option<String>,
    pub uploaded: Option<u64>,
}

fn descriptor_to_ffi(descriptor: BlobDescriptor) -> FfiBlobDescriptor {
    FfiBlobDescriptor {
        url: descriptor.url,
        sha256: descriptor.sha256.to_hex(),
        size: descriptor.size,
        mime_type: descriptor.mime_type,
        uploaded: descriptor.uploaded,
    }
}

/// `nmp_blossom::Sha256HexError` mirror -- the strict lowercase-hex parse
/// refusals, carried as a typed field by the variants that wrap it (never
/// flattened into a message string).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiBlossomSha256HexError {
    /// Not exactly 64 characters.
    BadLength { length: u64 },
    /// A character outside `[0-9a-f]` -- BUD-11 mandates LOWERCASE hex, so
    /// uppercase digits are refused, never case-folded.
    NotLowercaseHex { character: String },
}

fn sha256_hex_error_to_ffi(error: Sha256HexError) -> FfiBlossomSha256HexError {
    match error {
        Sha256HexError::BadLength { length } => FfiBlossomSha256HexError::BadLength {
            length: length as u64,
        },
        Sha256HexError::NotLowercaseHex { character } => {
            FfiBlossomSha256HexError::NotLowercaseHex {
                character: character.to_string(),
            }
        }
    }
}

/// `nmp_blossom::ServerUrlError` mirror -- every base-URL admission
/// refusal, variant-for-variant.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiBlossomServerUrlError {
    /// Not a parseable URL at all.
    Parse { reason: String },
    /// The URL names no host.
    MissingHost,
    /// A scheme other than `http`/`https`.
    UnsupportedScheme { scheme: String },
    /// URL userinfo would become an ambient credential; refused outright.
    Credentialed,
    /// BUD endpoints live at the domain root; a base URL with a path is
    /// refused rather than rewritten.
    NonRootPath { path: String },
    /// A query or fragment would survive into derived endpoint URLs;
    /// refused rather than stripped.
    QueryOrFragment,
}

fn server_url_error_to_ffi(error: ServerUrlError) -> FfiBlossomServerUrlError {
    match error {
        ServerUrlError::Parse { reason } => FfiBlossomServerUrlError::Parse { reason },
        ServerUrlError::MissingHost => FfiBlossomServerUrlError::MissingHost,
        ServerUrlError::UnsupportedScheme { scheme } => {
            FfiBlossomServerUrlError::UnsupportedScheme { scheme }
        }
        ServerUrlError::Credentialed => FfiBlossomServerUrlError::Credentialed,
        ServerUrlError::NonRootPath { path } => FfiBlossomServerUrlError::NonRootPath { path },
        ServerUrlError::QueryOrFragment => FfiBlossomServerUrlError::QueryOrFragment,
    }
}

/// One malformed signed BUD-03 `server` tag.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiBlossomServerListEntryError {
    MissingUrl,
    InvalidUrl { error: FfiBlossomServerUrlError },
}

/// Position-preserving malformed-tag evidence from a signed BUD-03 list.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiBlossomMalformedServerEntry {
    pub tag_index: u64,
    pub raw_url: Option<String>,
    pub error: FfiBlossomServerListEntryError,
}

/// Closed BUD-03 decode of one delivered canonical kind:10063 row.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiBlossomServerList {
    pub event_id: String,
    pub author_pubkey: String,
    /// Canonical, syntactically admitted URLs in exact signed-list order.
    pub servers: Vec<String>,
    /// Invalid `server` tags, retained rather than silently dropped.
    pub malformed_entries: Vec<FfiBlossomMalformedServerEntry>,
    pub server_tag_count: u64,
    pub unexpected_content: bool,
    pub spec_compliant: bool,
}

/// The active account's ordinary BUD-03 demand:
/// `kinds:[10063], authors:Reactive(ActivePubkey), AuthorOutboxes + Public`.
#[uniffi::export]
pub fn blossom_server_list_demand() -> FfiDemand {
    demand_to_ffi(nmp_blossom::active_account_server_list_demand())
}

/// Decode a delivered kind:10063 row. Replacement, deletion, expiry, absence,
/// account rerooting, source/access evidence, and caching remain the normal
/// observation/store path; this pure function adds no second lifecycle.
#[uniffi::export]
pub fn decode_blossom_server_list(row: FfiRow) -> FfiBlossomServerList {
    let decoded = nmp_blossom::decode_server_list_from_raw_tags(
        row.tags.iter().map(Vec::as_slice),
        &row.content,
    );
    FfiBlossomServerList {
        event_id: row.id,
        author_pubkey: row.pubkey,
        servers: decoded
            .servers()
            .iter()
            .map(|server| server.as_str().to_string())
            .collect(),
        malformed_entries: decoded
            .malformed_entries()
            .iter()
            .map(|entry| FfiBlossomMalformedServerEntry {
                tag_index: entry.tag_index as u64,
                raw_url: entry.raw_url.clone(),
                error: match &entry.reason {
                    MalformedServerEntryReason::MissingUrl => {
                        FfiBlossomServerListEntryError::MissingUrl
                    }
                    MalformedServerEntryReason::InvalidUrl(error) => {
                        FfiBlossomServerListEntryError::InvalidUrl {
                            error: server_url_error_to_ffi(error.clone()),
                        }
                    }
                },
            })
            .collect(),
        server_tag_count: decoded.server_tag_count() as u64,
        unexpected_content: decoded.has_unexpected_content(),
        spec_compliant: decoded.is_spec_compliant(),
    }
}

/// Explicit provenance-combination policy. No variant silently places local
/// configuration ahead of a signed BUD-03 first server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiBlossomServerCandidatePolicy {
    SignedListOnly,
    OperatorOnly,
    SignedListThenOperator,
}

fn candidate_policy_from_ffi(policy: FfiBlossomServerCandidatePolicy) -> ServerCandidatePolicy {
    match policy {
        FfiBlossomServerCandidatePolicy::SignedListOnly => ServerCandidatePolicy::SignedListOnly,
        FfiBlossomServerCandidatePolicy::OperatorOnly => ServerCandidatePolicy::OperatorOnly,
        FfiBlossomServerCandidatePolicy::SignedListThenOperator => {
            ServerCandidatePolicy::SignedListThenOperator
        }
    }
}

/// Authority that contributed one qualified candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiBlossomServerCandidateSource {
    SignedList,
    OperatorConfig,
}

fn candidate_source_to_ffi(source: ServerCandidateSource) -> FfiBlossomServerCandidateSource {
    match source {
        ServerCandidateSource::SignedList => FfiBlossomServerCandidateSource::SignedList,
        ServerCandidateSource::OperatorConfig => FfiBlossomServerCandidateSource::OperatorConfig,
    }
}

fn candidate_source_from_ffi(source: FfiBlossomServerCandidateSource) -> ServerCandidateSource {
    match source {
        FfiBlossomServerCandidateSource::SignedList => ServerCandidateSource::SignedList,
        FfiBlossomServerCandidateSource::OperatorConfig => ServerCandidateSource::OperatorConfig,
    }
}

/// Syntax plus network-admission evidence for one candidate. Only `Admitted`
/// is selectable; the actual HTTP operation repeats DNS admission to protect
/// against rebinding.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiBlossomServerAdmission {
    Admitted {
        resolved_addresses: Vec<String>,
        operator_local_override: bool,
    },
    InvalidUrl {
        error: FfiBlossomServerUrlError,
    },
    LocalHostNotAdmitted {
        host: String,
    },
    DnsRefused {
        reason: String,
    },
}

fn server_admission_to_ffi(admission: ServerAdmission) -> FfiBlossomServerAdmission {
    match admission {
        ServerAdmission::Admitted {
            resolved_addresses,
            operator_local_override,
        } => FfiBlossomServerAdmission::Admitted {
            resolved_addresses: resolved_addresses
                .into_iter()
                .map(|address| address.to_string())
                .collect(),
            operator_local_override,
        },
        ServerAdmission::Refused(ServerAdmissionRefusal::LocalHostNotAdmitted { host }) => {
            FfiBlossomServerAdmission::LocalHostNotAdmitted { host }
        }
        ServerAdmission::Refused(ServerAdmissionRefusal::DnsRefused { reason }) => {
            FfiBlossomServerAdmission::DnsRefused { reason }
        }
    }
}

/// One ordered, provenance-bearing qualification result.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiBlossomServerCandidateEvidence {
    pub server_url: String,
    pub source: FfiBlossomServerCandidateSource,
    pub admission: FfiBlossomServerAdmission,
}

/// Machinery failures before candidate qualification can run. Per-candidate
/// syntax/DNS/admission refusals are values in the returned evidence, not
/// thrown failures.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum FfiBlossomQualificationError {
    RuntimeUnavailable { reason: String },
    ClientBuild { reason: String },
}

impl std::fmt::Display for FfiBlossomQualificationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RuntimeUnavailable { reason } => {
                write!(f, "Blossom qualification runtime unavailable: {reason}")
            }
            Self::ClientBuild { reason } => {
                write!(f, "Blossom HTTP client construction failed: {reason}")
            }
        }
    }
}

impl std::error::Error for FfiBlossomQualificationError {}

/// `nmp_blossom::DescriptorError` mirror -- the strict BUD-02 descriptor
/// parse refusals, variant-for-variant.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiBlossomDescriptorError {
    /// Input exceeds the descriptor byte bound -- refused before any parse.
    TooLarge { limit_bytes: u64 },
    /// Not a JSON object of the expected field shapes.
    Json { reason: String },
    /// The mandatory `url` field is absent (or JSON `null`).
    MissingUrl,
    /// The mandatory `sha256` field is absent (or JSON `null`).
    MissingSha256,
    /// The mandatory `size` field is absent (or JSON `null`).
    MissingSize,
    /// `sha256` is present but not 64 lowercase hex characters.
    BadSha256 { error: FfiBlossomSha256HexError },
}

fn descriptor_error_to_ffi(error: DescriptorError) -> FfiBlossomDescriptorError {
    match error {
        DescriptorError::TooLarge { limit_bytes } => FfiBlossomDescriptorError::TooLarge {
            limit_bytes: limit_bytes as u64,
        },
        DescriptorError::Json { reason } => FfiBlossomDescriptorError::Json { reason },
        DescriptorError::MissingUrl => FfiBlossomDescriptorError::MissingUrl,
        DescriptorError::MissingSha256 => FfiBlossomDescriptorError::MissingSha256,
        DescriptorError::MissingSize => FfiBlossomDescriptorError::MissingSize,
        DescriptorError::BadSha256(error) => FfiBlossomDescriptorError::BadSha256 {
            error: sha256_hex_error_to_ffi(error),
        },
    }
}

/// The draft-construction + validation failure taxonomy
/// (`nmp_blossom::AuthDraftError` + `nmp_blossom::AuthValidationError`
/// mirrors, plus the boundary-re-introduced input parses). One enum for
/// both stages because both answer the same caller question -- "why is
/// there no usable authorization?" -- while every underlying check keeps
/// its own variant (a refused authorization still names the exact BUD-11
/// clause it failed).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum FfiBlossomAuthError {
    /// `author_pubkey_hex` is not a valid 32-byte hex public key
    /// (boundary-re-introduced fallibility; the direct-Rust builders take
    /// a proven `PublicKey`).
    InvalidAuthorPubkey { got: String },
    /// A supplied blob sha256 hex string failed the strict lowercase-hex
    /// parse (boundary-re-introduced fallibility).
    InvalidBlobSha256 { error: FfiBlossomSha256HexError },
    /// The signed authorization event did not parse as a nostr event --
    /// covers malformed JSON and malformed id/pubkey/sig hex, from either
    /// validation door (boundary-re-introduced fallibility).
    InvalidEventJson { reason: String },
    /// `nmp_blossom::AuthDraftError::ExpirationNotAfterCreatedAt` mirror:
    /// such a window is expired at birth.
    ExpirationNotAfterCreatedAt {
        created_at_secs: u64,
        expiration_secs: u64,
    },
    /// Not kind 24242.
    WrongKind { found: u16 },
    /// The Schnorr signature (or event id) does not verify.
    BadSignature { reason: String },
    /// No `t` tag with a value at all.
    MissingVerb,
    /// More than one `t` tag: a bearer token must grant exactly one verb.
    MultipleVerbs,
    /// The sole `t` tag names a different verb than expected (or a string
    /// that is not a BUD-11 verb).
    VerbMismatch {
        expected: FfiBlossomVerb,
        found: String,
    },
    /// The expectation binds a blob but no `x` tag carries exactly that
    /// lowercase-hex sha256.
    BlobNotBound { expected_sha256_hex: String },
    /// No `expiration` tag.
    MissingExpiration,
    /// The `expiration` tag is not in the future.
    Expired { expiration_secs: u64, now_secs: u64 },
    /// BUD-11 requires `created_at` in the past.
    CreatedAtInFuture { created_at_secs: u64, now_secs: u64 },
}

impl std::fmt::Display for FfiBlossomAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidAuthorPubkey { got } => {
                write!(f, "invalid author public key hex: {got:?}")
            }
            Self::InvalidBlobSha256 { error } => {
                write!(f, "invalid blob sha256 hex: {error:?}")
            }
            Self::InvalidEventJson { reason } => {
                write!(f, "authorization event does not parse: {reason}")
            }
            Self::ExpirationNotAfterCreatedAt {
                created_at_secs,
                expiration_secs,
            } => write!(
                f,
                "authorization expiration {expiration_secs} is not after created_at \
                 {created_at_secs}"
            ),
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
                "authorization verb {found:?} does not match expected {expected:?}"
            ),
            Self::BlobNotBound {
                expected_sha256_hex,
            } => write!(
                f,
                "authorization binds no `x` tag equal to {expected_sha256_hex}"
            ),
            Self::MissingExpiration => f.write_str("authorization event has no `expiration` tag"),
            Self::Expired {
                expiration_secs,
                now_secs,
            } => write!(
                f,
                "authorization expired: expiration {expiration_secs} is not after now {now_secs}"
            ),
            Self::CreatedAtInFuture {
                created_at_secs,
                now_secs,
            } => write!(
                f,
                "authorization created_at {created_at_secs} is after now {now_secs}"
            ),
        }
    }
}

impl std::error::Error for FfiBlossomAuthError {}

fn auth_draft_error_to_ffi(error: AuthDraftError) -> FfiBlossomAuthError {
    match error {
        AuthDraftError::ExpirationNotAfterCreatedAt {
            created_at,
            expiration,
        } => FfiBlossomAuthError::ExpirationNotAfterCreatedAt {
            created_at_secs: created_at.as_secs(),
            expiration_secs: expiration.as_secs(),
        },
    }
}

fn auth_validation_error_to_ffi(error: AuthValidationError) -> FfiBlossomAuthError {
    match error {
        AuthValidationError::WrongKind { found } => FfiBlossomAuthError::WrongKind { found },
        AuthValidationError::BadSignature { reason } => {
            FfiBlossomAuthError::BadSignature { reason }
        }
        AuthValidationError::MissingVerb => FfiBlossomAuthError::MissingVerb,
        AuthValidationError::MultipleVerbs => FfiBlossomAuthError::MultipleVerbs,
        AuthValidationError::VerbMismatch { expected, found } => {
            FfiBlossomAuthError::VerbMismatch {
                expected: verb_to_ffi(expected),
                found,
            }
        }
        AuthValidationError::BlobNotBound { expected } => FfiBlossomAuthError::BlobNotBound {
            expected_sha256_hex: expected.to_hex(),
        },
        AuthValidationError::MissingExpiration => FfiBlossomAuthError::MissingExpiration,
        AuthValidationError::Expired { expiration, now } => FfiBlossomAuthError::Expired {
            expiration_secs: expiration.as_secs(),
            now_secs: now.as_secs(),
        },
        AuthValidationError::CreatedAtInFuture { created_at, now } => {
            FfiBlossomAuthError::CreatedAtInFuture {
                created_at_secs: created_at.as_secs(),
                now_secs: now.as_secs(),
            }
        }
    }
}

fn parse_author_pubkey(hex: &str) -> Result<PublicKey, FfiBlossomAuthError> {
    PublicKey::from_hex(hex).map_err(|_| FfiBlossomAuthError::InvalidAuthorPubkey {
        got: hex.to_string(),
    })
}

fn parse_auth_blob_sha256(hex: &str) -> Result<Sha256Hash, FfiBlossomAuthError> {
    Sha256Hash::from_hex(hex).map_err(|error| FfiBlossomAuthError::InvalidBlobSha256 {
        error: sha256_hex_error_to_ffi(error),
    })
}

/// An UNSIGNED kind:24242 authorization draft. This module never signs --
/// the app does, through whichever signer path it has:
///
/// - engine sign-only path: feed `created_at_secs`/`kind`/`tags`/`content`
///   into `FfiSignEventRequest` and call `NmpEngine::sign_event` (the
///   author is frozen from the ACTIVE ACCOUNT, so `author_pubkey_hex`
///   passed to the draft builder must be that account's pubkey); the
///   resulting `FfiSignedEvent` enters
///   [`FfiBlossomAuthorization::validate_signed_event`];
/// - external/native signer path: hand `unsigned_event_json` to the
///   signer, then pass the signed event's canonical JSON to
///   [`FfiBlossomAuthorization::validate`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiBlossomAuthDraft {
    /// The draft as canonical unsigned-event JSON (`nostr::UnsignedEvent`),
    /// for external signers that consume event JSON.
    pub unsigned_event_json: String,
    /// The blob this draft binds via its `x` tag (`None` for `list`).
    pub blob_sha256_hex: Option<String>,
    /// The verb this draft grants.
    pub verb: FfiBlossomVerb,
    /// Raw tokens of the same draft, shaped for `FfiSignEventRequest` so
    /// the engine sign-only path needs no native JSON parsing. Always
    /// kind 24242.
    pub created_at_secs: u64,
    pub kind: u16,
    pub tags: Vec<Vec<String>>,
    pub content: String,
}

fn draft_to_ffi(
    unsigned: &UnsignedEvent,
    verb: FfiBlossomVerb,
    blob: Option<Sha256Hash>,
) -> FfiBlossomAuthDraft {
    FfiBlossomAuthDraft {
        unsigned_event_json: unsigned.as_json(),
        blob_sha256_hex: blob.map(|hash| hash.to_hex()),
        verb,
        created_at_secs: unsigned.created_at.as_secs(),
        kind: unsigned.kind.as_u16(),
        tags: unsigned
            .tags
            .iter()
            .map(|tag| tag.clone().to_vec())
            .collect(),
        content: unsigned.content.clone(),
    }
}

/// Compose an UNSIGNED BUD-11 `upload` authorization draft (kind 24242) --
/// `nmp_blossom::upload_authorization_draft` mirror. BUD-04 NOTE: a mirror
/// is authorized with THIS builder (the spec assigns mirroring the
/// `upload` verb); there is deliberately no separate mirror builder.
/// Engine-less free function ([`crate::entity`]'s precedent).
#[uniffi::export]
pub fn blossom_upload_authorization_draft(
    author_pubkey_hex: String,
    blob_sha256_hex: String,
    created_at_secs: u64,
    expiration_secs: u64,
    description: String,
) -> Result<FfiBlossomAuthDraft, FfiBlossomAuthError> {
    let author = parse_author_pubkey(&author_pubkey_hex)?;
    let blob = parse_auth_blob_sha256(&blob_sha256_hex)?;
    let unsigned = nmp_blossom::upload_authorization_draft(
        author,
        blob,
        Timestamp::from(created_at_secs),
        Timestamp::from(expiration_secs),
        &description,
    )
    .map_err(auth_draft_error_to_ffi)?;
    Ok(draft_to_ffi(&unsigned, FfiBlossomVerb::Upload, Some(blob)))
}

/// Compose an UNSIGNED BUD-12 `delete` authorization draft --
/// `nmp_blossom::delete_authorization_draft` mirror. Exactly ONE blob is
/// bound (BUD-12 forbids multi-blob deletes via extra `x` tags).
#[uniffi::export]
pub fn blossom_delete_authorization_draft(
    author_pubkey_hex: String,
    blob_sha256_hex: String,
    created_at_secs: u64,
    expiration_secs: u64,
    description: String,
) -> Result<FfiBlossomAuthDraft, FfiBlossomAuthError> {
    let author = parse_author_pubkey(&author_pubkey_hex)?;
    let blob = parse_auth_blob_sha256(&blob_sha256_hex)?;
    let unsigned = nmp_blossom::delete_authorization_draft(
        author,
        blob,
        Timestamp::from(created_at_secs),
        Timestamp::from(expiration_secs),
        &description,
    )
    .map_err(auth_draft_error_to_ffi)?;
    Ok(draft_to_ffi(&unsigned, FfiBlossomVerb::Delete, Some(blob)))
}

/// Compose an UNSIGNED BUD-12 `list` authorization draft --
/// `nmp_blossom::list_authorization_draft` mirror. No `x` tag: listing is
/// scoped to a pubkey by the request path, not to any blob.
#[uniffi::export]
pub fn blossom_list_authorization_draft(
    author_pubkey_hex: String,
    created_at_secs: u64,
    expiration_secs: u64,
    description: String,
) -> Result<FfiBlossomAuthDraft, FfiBlossomAuthError> {
    let author = parse_author_pubkey(&author_pubkey_hex)?;
    let unsigned = nmp_blossom::list_authorization_draft(
        author,
        Timestamp::from(created_at_secs),
        Timestamp::from(expiration_secs),
        &description,
    )
    .map_err(auth_draft_error_to_ffi)?;
    Ok(draft_to_ffi(&unsigned, FfiBlossomVerb::List, None))
}

/// A signed kind:24242 event PROVEN (at construction) to satisfy every
/// BUD-11 check -- `nmp_blossom::SignedAuthorization`'s
/// type-over-convention witness, carried across the boundary as an opaque
/// object so an unvalidated event can never enter a client operation.
#[derive(uniffi::Object)]
pub struct FfiBlossomAuthorization {
    inner: SignedAuthorization,
}

impl std::fmt::Debug for FfiBlossomAuthorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfiBlossomAuthorization")
            .field("verb", &self.inner.verb())
            .field("blob", &self.inner.blob())
            .finish()
    }
}

impl FfiBlossomAuthorization {
    /// The shared fail-closed door both constructors funnel through.
    fn validate_parsed(
        event: nostr::Event,
        verb: FfiBlossomVerb,
        blob_sha256_hex: Option<String>,
        now_secs: u64,
    ) -> Result<Arc<Self>, FfiBlossomAuthError> {
        let blob = blob_sha256_hex
            .as_deref()
            .map(parse_auth_blob_sha256)
            .transpose()?;
        let expected = ExpectedAuthorization {
            verb: verb_from_ffi(verb),
            blob,
        };
        let inner = SignedAuthorization::validate(event, &expected, Timestamp::from(now_secs))
            .map_err(auth_validation_error_to_ffi)?;
        Ok(Arc::new(Self { inner }))
    }
}

#[uniffi::export]
impl FfiBlossomAuthorization {
    /// Fail-closed BUD-11 validation of a signed kind:24242 event supplied
    /// as canonical event JSON (the external/native-signer path). `verb` is
    /// what the caller is ABOUT to use the authorization for; `blob_sha256_hex`
    /// binds the exact blob for verbs that grant one (upload/delete/mirror
    /// -- mirror validates under `Upload`); `now_secs` is the caller's
    /// clock (unix seconds).
    #[uniffi::constructor]
    pub fn validate(
        signed_event_json: String,
        verb: FfiBlossomVerb,
        blob_sha256_hex: Option<String>,
        now_secs: u64,
    ) -> Result<Arc<Self>, FfiBlossomAuthError> {
        let event = nostr::Event::from_json(&signed_event_json).map_err(|error| {
            FfiBlossomAuthError::InvalidEventJson {
                reason: error.to_string(),
            }
        })?;
        Self::validate_parsed(event, verb, blob_sha256_hex, now_secs)
    }

    /// Fail-closed BUD-11 validation of the exact value
    /// `NmpEngine::sign_event` returns (the engine sign-only path) -- the
    /// same checks as [`Self::validate`], with the event reassembled from
    /// its raw tokens through the same strict `nostr::Event` parser (a
    /// malformed id/sig/pubkey is a typed
    /// [`FfiBlossomAuthError::InvalidEventJson`], never a panic).
    #[uniffi::constructor]
    pub fn validate_signed_event(
        event: FfiSignedEvent,
        verb: FfiBlossomVerb,
        blob_sha256_hex: Option<String>,
        now_secs: u64,
    ) -> Result<Arc<Self>, FfiBlossomAuthError> {
        let json = serde_json::json!({
            "id": event.id,
            "pubkey": event.pubkey,
            "created_at": event.created_at,
            "kind": event.kind,
            "tags": event.tags,
            "content": event.content,
            "sig": event.sig,
        })
        .to_string();
        let event = nostr::Event::from_json(&json).map_err(|error| {
            FfiBlossomAuthError::InvalidEventJson {
                reason: error.to_string(),
            }
        })?;
        Self::validate_parsed(event, verb, blob_sha256_hex, now_secs)
    }

    /// The verb this authorization was validated FOR.
    pub fn verb(&self) -> FfiBlossomVerb {
        verb_to_ffi(self.inner.verb())
    }

    /// The blob hash this authorization was proven to bind (`None` for
    /// verbs validated without a blob binding).
    pub fn blob_sha256_hex(&self) -> Option<String> {
        self.inner.blob().map(|hash| hash.to_hex())
    }
}

/// [`FfiBlossomClient`] construction knobs
/// (`nmp_blossom::BlossomClientConfig` mirror). `None` means the crate
/// default.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiBlossomClientConfig {
    /// Operator opt-in local-host allowlist, in `nmp-transport`'s
    /// normalized bare-host form (lowercase, no brackets) -- the same
    /// vocabulary the engine's `allowed_local_relay_hosts` uses. Empty
    /// means NO loopback/private/link-local/onion host may be dialed.
    pub allowed_local_hosts: Vec<String>,
    /// Cap on a single-descriptor response body (upload/mirror).
    pub max_response_bytes: Option<u64>,
    /// Cap on a `GET /list` response body.
    pub max_list_response_bytes: Option<u64>,
    /// Overall request deadline (connect, headers, and body), seconds.
    pub request_deadline_secs: Option<u64>,
}

fn byte_bound(bound: u64) -> usize {
    usize::try_from(bound).unwrap_or(usize::MAX)
}

fn client_config_from_ffi(config: FfiBlossomClientConfig) -> BlossomClientConfig {
    BlossomClientConfig {
        allowed_local_hosts: config
            .allowed_local_hosts
            .into_iter()
            .collect::<BTreeSet<String>>(),
        max_response_bytes: config
            .max_response_bytes
            .map(byte_bound)
            .unwrap_or(DEFAULT_MAX_RESPONSE_BYTES),
        max_list_response_bytes: config
            .max_list_response_bytes
            .map(byte_bound)
            .unwrap_or(DEFAULT_MAX_LIST_RESPONSE_BYTES),
        request_deadline: config
            .request_deadline_secs
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_REQUEST_DEADLINE),
    }
}

/// `nmp_blossom::UploadError` mirror plus the boundary-re-introduced
/// parse/machinery variants. Exhaustive; never collapsed with the other
/// operations' taxonomies.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum FfiBlossomUploadError {
    /// `server_url` failed [`FfiBlossomServerUrlError`]'s admission rules
    /// (boundary-re-introduced fallibility).
    InvalidServerUrl { error: FfiBlossomServerUrlError },
    /// The per-call current-thread tokio runtime could not be built.
    RuntimeUnavailable { reason: String },
    /// `nmp_blossom::ClientBuildError` mirror: the HTTP stack could not be
    /// constructed (system DNS configuration unreadable, or reqwest client
    /// construction failed).
    ClientBuild { reason: String },
    /// The supplied authorization is not an `upload` grant for EXACTLY the
    /// blob bytes being uploaded -- refused before any admission or I/O.
    AuthorizationBlobMismatch {
        expected_sha256_hex: String,
        authorized_verb: FfiBlossomVerb,
        authorized_blob_sha256_hex: Option<String>,
    },
    /// Literal loopback/private/link-local/unspecified/onion host without
    /// operator opt-in -- refused before ANY socket I/O.
    LocalHostNotAdmitted { host: String },
    /// Transport failure: connect/DNS/TLS/timeout, or the body stream died.
    Network { detail: String },
    /// The server answered with a redirect; redirects are never followed.
    RedirectRefused { status: u16 },
    /// 401/403: the server refused the authorization itself.
    AuthRejected { status: u16, reason: Option<String> },
    /// Any other non-success, non-5xx status.
    ServerRejected { status: u16, reason: Option<String> },
    /// 5xx: the server failed.
    ServerError { status: u16, reason: Option<String> },
    /// The success body exceeded the configured response cap.
    ResponseTooLarge { limit_bytes: u64 },
    /// The success body is not a valid BUD-02 blob descriptor.
    DescriptorInvalid { error: FfiBlossomDescriptorError },
    /// INTEGRITY GATE: the server's descriptor names a different sha256
    /// than the bytes this client just hashed and sent -- fail closed.
    Sha256Mismatch {
        expected_sha256_hex: String,
        returned_sha256_hex: String,
    },
}

impl std::fmt::Display for FfiBlossomUploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidServerUrl { error } => {
                write!(f, "invalid Blossom server URL: {error:?}")
            }
            Self::RuntimeUnavailable { reason } => {
                write!(f, "Blossom upload runtime unavailable: {reason}")
            }
            Self::ClientBuild { reason } => {
                write!(f, "Blossom HTTP client construction failed: {reason}")
            }
            Self::AuthorizationBlobMismatch {
                expected_sha256_hex,
                authorized_verb,
                authorized_blob_sha256_hex,
            } => write!(
                f,
                "authorization ({authorized_verb:?}, blob {authorized_blob_sha256_hex:?}) does \
                 not grant uploading blob {expected_sha256_hex}"
            ),
            Self::LocalHostNotAdmitted { host } => write!(
                f,
                "refusing Blossom upload: host {host:?} is local and not operator opted-in"
            ),
            Self::Network { detail } => write!(f, "Blossom upload transport failed: {detail}"),
            Self::RedirectRefused { status } => {
                write!(
                    f,
                    "Blossom upload redirects are not followed (HTTP {status})"
                )
            }
            Self::AuthRejected { status, reason } => write!(
                f,
                "Blossom server rejected the authorization (HTTP {status}, reason {reason:?})"
            ),
            Self::ServerRejected { status, reason } => write!(
                f,
                "Blossom server rejected the upload (HTTP {status}, reason {reason:?})"
            ),
            Self::ServerError { status, reason } => write!(
                f,
                "Blossom server failed (HTTP {status}, reason {reason:?})"
            ),
            Self::ResponseTooLarge { limit_bytes } => {
                write!(f, "Blossom descriptor response exceeds {limit_bytes} bytes")
            }
            Self::DescriptorInvalid { error } => {
                write!(f, "Blossom descriptor invalid: {error:?}")
            }
            Self::Sha256Mismatch {
                expected_sha256_hex,
                returned_sha256_hex,
            } => write!(
                f,
                "Blossom server returned sha256 {returned_sha256_hex} for a blob hashing to \
                 {expected_sha256_hex} -- refusing the descriptor"
            ),
        }
    }
}

impl std::error::Error for FfiBlossomUploadError {}

fn upload_error_to_ffi(error: UploadError) -> FfiBlossomUploadError {
    match error {
        UploadError::AuthorizationBlobMismatch {
            expected,
            authorized_verb,
            authorized_blob,
        } => FfiBlossomUploadError::AuthorizationBlobMismatch {
            expected_sha256_hex: expected.to_hex(),
            authorized_verb: verb_to_ffi(authorized_verb),
            authorized_blob_sha256_hex: authorized_blob.map(|hash| hash.to_hex()),
        },
        UploadError::LocalHostNotAdmitted { host } => {
            FfiBlossomUploadError::LocalHostNotAdmitted { host }
        }
        UploadError::Network { detail } => FfiBlossomUploadError::Network { detail },
        UploadError::RedirectRefused { status } => {
            FfiBlossomUploadError::RedirectRefused { status }
        }
        UploadError::AuthRejected { status, reason } => {
            FfiBlossomUploadError::AuthRejected { status, reason }
        }
        UploadError::ServerRejected { status, reason } => {
            FfiBlossomUploadError::ServerRejected { status, reason }
        }
        UploadError::ServerError { status, reason } => {
            FfiBlossomUploadError::ServerError { status, reason }
        }
        UploadError::ResponseTooLarge { limit_bytes } => FfiBlossomUploadError::ResponseTooLarge {
            limit_bytes: limit_bytes as u64,
        },
        UploadError::DescriptorInvalid(error) => FfiBlossomUploadError::DescriptorInvalid {
            error: descriptor_error_to_ffi(error),
        },
        UploadError::Sha256Mismatch { expected, returned } => {
            FfiBlossomUploadError::Sha256Mismatch {
                expected_sha256_hex: expected.to_hex(),
                returned_sha256_hex: returned.to_hex(),
            }
        }
    }
}

/// `nmp_blossom::MirrorError` mirror plus the boundary-re-introduced
/// parse/machinery variants. Deliberately its own enum: mirror has failure
/// modes with no upload analogue (the 409 hash refusal, the 502
/// origin-fetch failure) and operation failures are never collapsed.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum FfiBlossomMirrorError {
    /// `server_url` failed the admission rules (boundary parse).
    InvalidServerUrl { error: FfiBlossomServerUrlError },
    /// `expected_sha256_hex` failed the strict lowercase-hex parse
    /// (boundary parse).
    InvalidExpectedSha256 { error: FfiBlossomSha256HexError },
    /// The per-call current-thread tokio runtime could not be built.
    RuntimeUnavailable { reason: String },
    /// `nmp_blossom::ClientBuildError` mirror.
    ClientBuild { reason: String },
    /// Not an `upload` grant (BUD-04 mirrors under the `upload` verb)
    /// bound to EXACTLY the expected blob -- refused before any I/O.
    AuthorizationBlobMismatch {
        expected_sha256_hex: String,
        authorized_verb: FfiBlossomVerb,
        authorized_blob_sha256_hex: Option<String>,
    },
    /// Unadmitted literal-local host -- refused before ANY socket I/O.
    LocalHostNotAdmitted { host: String },
    /// Transport failure.
    Network { detail: String },
    /// Redirects are never followed.
    RedirectRefused { status: u16 },
    /// 401/403: the server refused the authorization itself.
    AuthRejected { status: u16, reason: Option<String> },
    /// 409: the SERVER found the mirrored blob's hash does not match the
    /// authorized `x` tag -- distinct from `Sha256Mismatch`, which is THIS
    /// CLIENT refusing a 2xx descriptor.
    HashMismatchRefused { reason: Option<String> },
    /// 502: the destination server could not fetch the source URL -- the
    /// origin failed, not the destination.
    OriginFetchFailed { reason: Option<String> },
    /// Any other non-success, non-5xx status.
    ServerRejected { status: u16, reason: Option<String> },
    /// Any 5xx other than 502: the destination server itself failed.
    ServerError { status: u16, reason: Option<String> },
    /// The success body exceeded the configured response cap.
    ResponseTooLarge { limit_bytes: u64 },
    /// The success body is not a valid BUD-02 blob descriptor.
    DescriptorInvalid { error: FfiBlossomDescriptorError },
    /// INTEGRITY GATE: the 2xx descriptor names a different sha256 than
    /// the hash this client authorized -- fail closed.
    Sha256Mismatch {
        expected_sha256_hex: String,
        returned_sha256_hex: String,
    },
}

impl std::fmt::Display for FfiBlossomMirrorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidServerUrl { error } => {
                write!(f, "invalid Blossom server URL: {error:?}")
            }
            Self::InvalidExpectedSha256 { error } => {
                write!(f, "invalid expected sha256 hex: {error:?}")
            }
            Self::RuntimeUnavailable { reason } => {
                write!(f, "Blossom mirror runtime unavailable: {reason}")
            }
            Self::ClientBuild { reason } => {
                write!(f, "Blossom HTTP client construction failed: {reason}")
            }
            Self::AuthorizationBlobMismatch {
                expected_sha256_hex,
                authorized_verb,
                authorized_blob_sha256_hex,
            } => write!(
                f,
                "authorization ({authorized_verb:?}, blob {authorized_blob_sha256_hex:?}) does \
                 not grant mirroring blob {expected_sha256_hex}"
            ),
            Self::LocalHostNotAdmitted { host } => write!(
                f,
                "refusing Blossom mirror: host {host:?} is local and not operator opted-in"
            ),
            Self::Network { detail } => write!(f, "Blossom mirror transport failed: {detail}"),
            Self::RedirectRefused { status } => {
                write!(
                    f,
                    "Blossom mirror redirects are not followed (HTTP {status})"
                )
            }
            Self::AuthRejected { status, reason } => write!(
                f,
                "Blossom server rejected the authorization (HTTP {status}, reason {reason:?})"
            ),
            Self::HashMismatchRefused { reason } => write!(
                f,
                "Blossom server refused the mirror: mirrored blob hash does not match the \
                 authorized x tag (HTTP 409, reason {reason:?})"
            ),
            Self::OriginFetchFailed { reason } => write!(
                f,
                "Blossom server could not fetch the mirror source URL (HTTP 502, reason \
                 {reason:?})"
            ),
            Self::ServerRejected { status, reason } => write!(
                f,
                "Blossom server rejected the mirror (HTTP {status}, reason {reason:?})"
            ),
            Self::ServerError { status, reason } => write!(
                f,
                "Blossom server failed (HTTP {status}, reason {reason:?})"
            ),
            Self::ResponseTooLarge { limit_bytes } => {
                write!(f, "Blossom descriptor response exceeds {limit_bytes} bytes")
            }
            Self::DescriptorInvalid { error } => {
                write!(f, "Blossom descriptor invalid: {error:?}")
            }
            Self::Sha256Mismatch {
                expected_sha256_hex,
                returned_sha256_hex,
            } => write!(
                f,
                "Blossom server returned sha256 {returned_sha256_hex} for a mirror authorized \
                 as {expected_sha256_hex} -- refusing the descriptor"
            ),
        }
    }
}

impl std::error::Error for FfiBlossomMirrorError {}

fn mirror_error_to_ffi(error: MirrorError) -> FfiBlossomMirrorError {
    match error {
        MirrorError::AuthorizationBlobMismatch {
            expected,
            authorized_verb,
            authorized_blob,
        } => FfiBlossomMirrorError::AuthorizationBlobMismatch {
            expected_sha256_hex: expected.to_hex(),
            authorized_verb: verb_to_ffi(authorized_verb),
            authorized_blob_sha256_hex: authorized_blob.map(|hash| hash.to_hex()),
        },
        MirrorError::LocalHostNotAdmitted { host } => {
            FfiBlossomMirrorError::LocalHostNotAdmitted { host }
        }
        MirrorError::Network { detail } => FfiBlossomMirrorError::Network { detail },
        MirrorError::RedirectRefused { status } => {
            FfiBlossomMirrorError::RedirectRefused { status }
        }
        MirrorError::AuthRejected { status, reason } => {
            FfiBlossomMirrorError::AuthRejected { status, reason }
        }
        MirrorError::HashMismatchRefused { reason } => {
            FfiBlossomMirrorError::HashMismatchRefused { reason }
        }
        MirrorError::OriginFetchFailed { reason } => {
            FfiBlossomMirrorError::OriginFetchFailed { reason }
        }
        MirrorError::ServerRejected { status, reason } => {
            FfiBlossomMirrorError::ServerRejected { status, reason }
        }
        MirrorError::ServerError { status, reason } => {
            FfiBlossomMirrorError::ServerError { status, reason }
        }
        MirrorError::ResponseTooLarge { limit_bytes } => FfiBlossomMirrorError::ResponseTooLarge {
            limit_bytes: limit_bytes as u64,
        },
        MirrorError::DescriptorInvalid(error) => FfiBlossomMirrorError::DescriptorInvalid {
            error: descriptor_error_to_ffi(error),
        },
        MirrorError::Sha256Mismatch { expected, returned } => {
            FfiBlossomMirrorError::Sha256Mismatch {
                expected_sha256_hex: expected.to_hex(),
                returned_sha256_hex: returned.to_hex(),
            }
        }
    }
}

/// `nmp_blossom::DeleteError` mirror plus the boundary-re-introduced
/// parse/machinery variants. Exhaustive.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum FfiBlossomDeleteError {
    /// `server_url` failed the admission rules (boundary parse).
    InvalidServerUrl { error: FfiBlossomServerUrlError },
    /// `blob_sha256_hex` failed the strict lowercase-hex parse (boundary
    /// parse).
    InvalidBlobSha256 { error: FfiBlossomSha256HexError },
    /// The per-call current-thread tokio runtime could not be built.
    RuntimeUnavailable { reason: String },
    /// `nmp_blossom::ClientBuildError` mirror.
    ClientBuild { reason: String },
    /// Not a `delete` grant for EXACTLY the blob named in the request path
    /// -- refused before any admission or I/O.
    AuthorizationBlobMismatch {
        expected_sha256_hex: String,
        authorized_verb: FfiBlossomVerb,
        authorized_blob_sha256_hex: Option<String>,
    },
    /// Unadmitted literal-local host -- refused before ANY socket I/O.
    LocalHostNotAdmitted { host: String },
    /// Transport failure.
    Network { detail: String },
    /// Redirects are never followed.
    RedirectRefused { status: u16 },
    /// 401/402/403: the server refused the authorization (BUD-12 names all
    /// three as authorization-indicting for DELETE).
    AuthRejected { status: u16, reason: Option<String> },
    /// 404: no blob with this sha256 exists on the server -- "already
    /// gone" is actionable where a generic rejection is not.
    NotFound { reason: Option<String> },
    /// Any other non-success, non-5xx status.
    ServerRejected { status: u16, reason: Option<String> },
    /// 5xx: the server failed.
    ServerError { status: u16, reason: Option<String> },
}

impl std::fmt::Display for FfiBlossomDeleteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidServerUrl { error } => {
                write!(f, "invalid Blossom server URL: {error:?}")
            }
            Self::InvalidBlobSha256 { error } => {
                write!(f, "invalid blob sha256 hex: {error:?}")
            }
            Self::RuntimeUnavailable { reason } => {
                write!(f, "Blossom delete runtime unavailable: {reason}")
            }
            Self::ClientBuild { reason } => {
                write!(f, "Blossom HTTP client construction failed: {reason}")
            }
            Self::AuthorizationBlobMismatch {
                expected_sha256_hex,
                authorized_verb,
                authorized_blob_sha256_hex,
            } => write!(
                f,
                "authorization ({authorized_verb:?}, blob {authorized_blob_sha256_hex:?}) does \
                 not grant deleting blob {expected_sha256_hex}"
            ),
            Self::LocalHostNotAdmitted { host } => write!(
                f,
                "refusing Blossom delete: host {host:?} is local and not operator opted-in"
            ),
            Self::Network { detail } => write!(f, "Blossom delete transport failed: {detail}"),
            Self::RedirectRefused { status } => {
                write!(
                    f,
                    "Blossom delete redirects are not followed (HTTP {status})"
                )
            }
            Self::AuthRejected { status, reason } => write!(
                f,
                "Blossom server rejected the authorization (HTTP {status}, reason {reason:?})"
            ),
            Self::NotFound { reason } => write!(
                f,
                "Blossom server has no such blob (HTTP 404, reason {reason:?})"
            ),
            Self::ServerRejected { status, reason } => write!(
                f,
                "Blossom server rejected the delete (HTTP {status}, reason {reason:?})"
            ),
            Self::ServerError { status, reason } => write!(
                f,
                "Blossom server failed (HTTP {status}, reason {reason:?})"
            ),
        }
    }
}

impl std::error::Error for FfiBlossomDeleteError {}

fn delete_error_to_ffi(error: DeleteError) -> FfiBlossomDeleteError {
    match error {
        DeleteError::AuthorizationBlobMismatch {
            expected,
            authorized_verb,
            authorized_blob,
        } => FfiBlossomDeleteError::AuthorizationBlobMismatch {
            expected_sha256_hex: expected.to_hex(),
            authorized_verb: verb_to_ffi(authorized_verb),
            authorized_blob_sha256_hex: authorized_blob.map(|hash| hash.to_hex()),
        },
        DeleteError::LocalHostNotAdmitted { host } => {
            FfiBlossomDeleteError::LocalHostNotAdmitted { host }
        }
        DeleteError::Network { detail } => FfiBlossomDeleteError::Network { detail },
        DeleteError::RedirectRefused { status } => {
            FfiBlossomDeleteError::RedirectRefused { status }
        }
        DeleteError::AuthRejected { status, reason } => {
            FfiBlossomDeleteError::AuthRejected { status, reason }
        }
        DeleteError::NotFound { reason } => FfiBlossomDeleteError::NotFound { reason },
        DeleteError::ServerRejected { status, reason } => {
            FfiBlossomDeleteError::ServerRejected { status, reason }
        }
        DeleteError::ServerError { status, reason } => {
            FfiBlossomDeleteError::ServerError { status, reason }
        }
    }
}

/// `nmp_blossom::ListError` mirror plus the boundary-re-introduced
/// parse/machinery variants. Exhaustive.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum FfiBlossomListError {
    /// `server_url` failed the admission rules (boundary parse).
    InvalidServerUrl { error: FfiBlossomServerUrlError },
    /// `owner_pubkey_hex` is not a valid 32-byte hex public key (boundary
    /// parse).
    InvalidOwnerPubkey { got: String },
    /// `cursor_sha256_hex` failed the strict lowercase-hex parse (boundary
    /// parse).
    InvalidCursor { error: FfiBlossomSha256HexError },
    /// The per-call current-thread tokio runtime could not be built.
    RuntimeUnavailable { reason: String },
    /// `nmp_blossom::ClientBuildError` mirror.
    ClientBuild { reason: String },
    /// An authorization was supplied but is not a `list` grant -- refused
    /// before any admission or I/O (no cross-verb replay).
    WrongVerb { authorized_verb: FfiBlossomVerb },
    /// Unadmitted literal-local host -- refused before ANY socket I/O.
    LocalHostNotAdmitted { host: String },
    /// Transport failure.
    Network { detail: String },
    /// Redirects are never followed.
    RedirectRefused { status: u16 },
    /// 401/402/403: the server refused the request as unauthorized --
    /// including a 401 on an auth-less call to a server that requires a
    /// `list` authorization.
    AuthRejected { status: u16, reason: Option<String> },
    /// Any other non-success, non-5xx status.
    ServerRejected { status: u16, reason: Option<String> },
    /// 5xx: the server failed.
    ServerError { status: u16, reason: Option<String> },
    /// The success body exceeded the configured list response cap.
    ResponseTooLarge { limit_bytes: u64 },
    /// The success body is not a top-level JSON array -- refused outright,
    /// never coerced.
    BodyNotAnArray { reason: String },
    /// Array element `index` failed the strict BUD-02 descriptor rules --
    /// the WHOLE list fails typed; a malformed row is never silently
    /// skipped.
    InvalidDescriptor {
        index: u64,
        error: FfiBlossomDescriptorError,
    },
}

impl std::fmt::Display for FfiBlossomListError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidServerUrl { error } => {
                write!(f, "invalid Blossom server URL: {error:?}")
            }
            Self::InvalidOwnerPubkey { got } => {
                write!(f, "invalid owner public key hex: {got:?}")
            }
            Self::InvalidCursor { error } => {
                write!(f, "invalid list cursor sha256 hex: {error:?}")
            }
            Self::RuntimeUnavailable { reason } => {
                write!(f, "Blossom list runtime unavailable: {reason}")
            }
            Self::ClientBuild { reason } => {
                write!(f, "Blossom HTTP client construction failed: {reason}")
            }
            Self::WrongVerb { authorized_verb } => write!(
                f,
                "authorization verb {authorized_verb:?} does not grant listing (need `list`)"
            ),
            Self::LocalHostNotAdmitted { host } => write!(
                f,
                "refusing Blossom list: host {host:?} is local and not operator opted-in"
            ),
            Self::Network { detail } => write!(f, "Blossom list transport failed: {detail}"),
            Self::RedirectRefused { status } => {
                write!(f, "Blossom list redirects are not followed (HTTP {status})")
            }
            Self::AuthRejected { status, reason } => write!(
                f,
                "Blossom server rejected the authorization (HTTP {status}, reason {reason:?})"
            ),
            Self::ServerRejected { status, reason } => write!(
                f,
                "Blossom server rejected the list (HTTP {status}, reason {reason:?})"
            ),
            Self::ServerError { status, reason } => write!(
                f,
                "Blossom server failed (HTTP {status}, reason {reason:?})"
            ),
            Self::ResponseTooLarge { limit_bytes } => {
                write!(f, "Blossom list response exceeds {limit_bytes} bytes")
            }
            Self::BodyNotAnArray { reason } => {
                write!(f, "Blossom list body is not a JSON array: {reason}")
            }
            Self::InvalidDescriptor { index, error } => write!(
                f,
                "Blossom list element {index} is not a valid blob descriptor: {error:?}"
            ),
        }
    }
}

impl std::error::Error for FfiBlossomListError {}

fn list_error_to_ffi(error: ListError) -> FfiBlossomListError {
    match error {
        ListError::WrongVerb { authorized_verb } => FfiBlossomListError::WrongVerb {
            authorized_verb: verb_to_ffi(authorized_verb),
        },
        ListError::LocalHostNotAdmitted { host } => {
            FfiBlossomListError::LocalHostNotAdmitted { host }
        }
        ListError::Network { detail } => FfiBlossomListError::Network { detail },
        ListError::RedirectRefused { status } => FfiBlossomListError::RedirectRefused { status },
        ListError::AuthRejected { status, reason } => {
            FfiBlossomListError::AuthRejected { status, reason }
        }
        ListError::ServerRejected { status, reason } => {
            FfiBlossomListError::ServerRejected { status, reason }
        }
        ListError::ServerError { status, reason } => {
            FfiBlossomListError::ServerError { status, reason }
        }
        ListError::ResponseTooLarge { limit_bytes } => FfiBlossomListError::ResponseTooLarge {
            limit_bytes: limit_bytes as u64,
        },
        ListError::BodyNotAnArray { reason } => FfiBlossomListError::BodyNotAnArray { reason },
        ListError::InvalidDescriptor { index, source } => FfiBlossomListError::InvalidDescriptor {
            index: index as u64,
            error: descriptor_error_to_ffi(source),
        },
    }
}

/// One current-thread tokio runtime per client call
/// (`nmp-engine/src/relay_information.rs`'s `http_runtime()` discipline,
/// mirrored deliberately): the reqwest/hickory stack is constructed AND
/// dropped inside this flight's own runtime, so no runtime-bound DNS or
/// connection state can leak into the next call.
fn blocking_runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("Blossom HTTP runtime: {error}"))
}

/// The BUD-02/04/12 blob client (`nmp_blossom::BlossomClient` mirror).
/// Every method BLOCKS the calling thread for up to the configured request
/// deadline -- call OFF the main thread (the hand-written Swift/Kotlin
/// wrappers dispatch for you). Construction is infallible: the config is
/// held as data and the HTTP stack is built per call (see module doc), so
/// construction failures surface as each operation's own typed
/// `ClientBuild` variant.
#[derive(uniffi::Object)]
pub struct FfiBlossomClient {
    config: BlossomClientConfig,
}

impl FfiBlossomClient {
    fn parse_server<E>(
        server_url: &str,
        wrap: impl FnOnce(FfiBlossomServerUrlError) -> E,
    ) -> Result<BlossomServerUrl, E> {
        BlossomServerUrl::parse(server_url).map_err(|error| wrap(server_url_error_to_ffi(error)))
    }
}

#[uniffi::export]
impl FfiBlossomClient {
    #[uniffi::constructor]
    pub fn new(config: FfiBlossomClientConfig) -> Arc<Self> {
        Arc::new(Self {
            config: client_config_from_ffi(config),
        })
    }

    /// BLOCKING endpoint qualification with explicit configuration/list
    /// combination and per-candidate provenance. No HTTP request is sent.
    /// `InvalidUrl`/local-host/DNS refusals remain evidence values so callers
    /// can explain every candidate rather than catching one aggregate error.
    /// Call off the main thread.
    pub fn qualify_server_candidates(
        &self,
        policy: FfiBlossomServerCandidatePolicy,
        operator_server_urls: Vec<String>,
        signed_list: Option<FfiBlossomServerList>,
    ) -> Result<Vec<FfiBlossomServerCandidateEvidence>, FfiBlossomQualificationError> {
        let policy = candidate_policy_from_ffi(policy);
        let mut candidates = Vec::new();
        if matches!(
            policy,
            ServerCandidatePolicy::SignedListOnly | ServerCandidatePolicy::SignedListThenOperator
        ) {
            if let Some(list) = signed_list {
                candidates.extend(
                    list.servers
                        .into_iter()
                        .map(|url| (FfiBlossomServerCandidateSource::SignedList, url)),
                );
            }
        }
        if matches!(
            policy,
            ServerCandidatePolicy::OperatorOnly | ServerCandidatePolicy::SignedListThenOperator
        ) {
            candidates.extend(
                operator_server_urls
                    .into_iter()
                    .map(|url| (FfiBlossomServerCandidateSource::OperatorConfig, url)),
            );
        }

        let runtime = blocking_runtime()
            .map_err(|reason| FfiBlossomQualificationError::RuntimeUnavailable { reason })?;
        runtime.block_on(async {
            let client = BlossomClient::new(self.config.clone()).map_err(|error| {
                FfiBlossomQualificationError::ClientBuild {
                    reason: error.reason,
                }
            })?;
            let mut evidence = Vec::with_capacity(candidates.len());
            for (source, raw_url) in candidates {
                match BlossomServerUrl::parse(&raw_url) {
                    Ok(server) => {
                        let qualified = client
                            .qualify_server_candidate(candidate_source_from_ffi(source), server)
                            .await;
                        evidence.push(FfiBlossomServerCandidateEvidence {
                            server_url: qualified.server.as_str().to_string(),
                            source: candidate_source_to_ffi(qualified.source),
                            admission: server_admission_to_ffi(qualified.admission),
                        });
                    }
                    Err(error) => {
                        evidence.push(FfiBlossomServerCandidateEvidence {
                            server_url: raw_url,
                            source,
                            admission: FfiBlossomServerAdmission::InvalidUrl {
                                error: server_url_error_to_ffi(error),
                            },
                        });
                    }
                }
            }
            Ok(evidence)
        })
    }

    /// BLOCKING `PUT /upload` of `blob`'s exact bytes -- self-verifying end
    /// to end (`nmp_blossom::BlossomClient::upload`): the returned
    /// descriptor's sha256 was PROVEN equal to the hash of the uploaded
    /// bytes. Call off the main thread.
    pub fn upload(
        &self,
        server_url: String,
        blob: Vec<u8>,
        content_type: Option<String>,
        auth: Arc<FfiBlossomAuthorization>,
    ) -> Result<FfiBlobDescriptor, FfiBlossomUploadError> {
        let server = Self::parse_server(&server_url, |error| {
            FfiBlossomUploadError::InvalidServerUrl { error }
        })?;
        let runtime = blocking_runtime()
            .map_err(|reason| FfiBlossomUploadError::RuntimeUnavailable { reason })?;
        runtime.block_on(async {
            let client = BlossomClient::new(self.config.clone()).map_err(|error| {
                FfiBlossomUploadError::ClientBuild {
                    reason: error.reason,
                }
            })?;
            let verified = client
                .upload(&server, &blob, content_type.as_deref(), &auth.inner)
                .await
                .map_err(upload_error_to_ffi)?;
            Ok(descriptor_to_ffi(verified.into_descriptor()))
        })
    }

    /// BLOCKING `PUT /mirror` (BUD-04): ask `server_url` to download the
    /// blob at `source_url` itself, integrity-gated against
    /// `expected_sha256_hex` (`nmp_blossom::BlossomClient::mirror`). The
    /// authorization is an `upload` grant bound to that hash. Call off the
    /// main thread.
    pub fn mirror(
        &self,
        server_url: String,
        source_url: String,
        expected_sha256_hex: String,
        auth: Arc<FfiBlossomAuthorization>,
    ) -> Result<FfiBlobDescriptor, FfiBlossomMirrorError> {
        let server = Self::parse_server(&server_url, |error| {
            FfiBlossomMirrorError::InvalidServerUrl { error }
        })?;
        let expected = Sha256Hash::from_hex(&expected_sha256_hex).map_err(|error| {
            FfiBlossomMirrorError::InvalidExpectedSha256 {
                error: sha256_hex_error_to_ffi(error),
            }
        })?;
        let runtime = blocking_runtime()
            .map_err(|reason| FfiBlossomMirrorError::RuntimeUnavailable { reason })?;
        runtime.block_on(async {
            let client = BlossomClient::new(self.config.clone()).map_err(|error| {
                FfiBlossomMirrorError::ClientBuild {
                    reason: error.reason,
                }
            })?;
            let verified = client
                .mirror(&server, &source_url, expected, &auth.inner)
                .await
                .map_err(mirror_error_to_ffi)?;
            Ok(descriptor_to_ffi(verified.into_descriptor()))
        })
    }

    /// BLOCKING `DELETE /<sha256>` (BUD-12)
    /// (`nmp_blossom::BlossomClient::delete`). The authorization is a
    /// `delete` grant bound to EXACTLY `blob_sha256_hex`. Call off the main
    /// thread.
    pub fn delete(
        &self,
        server_url: String,
        blob_sha256_hex: String,
        auth: Arc<FfiBlossomAuthorization>,
    ) -> Result<(), FfiBlossomDeleteError> {
        let server = Self::parse_server(&server_url, |error| {
            FfiBlossomDeleteError::InvalidServerUrl { error }
        })?;
        let blob = Sha256Hash::from_hex(&blob_sha256_hex).map_err(|error| {
            FfiBlossomDeleteError::InvalidBlobSha256 {
                error: sha256_hex_error_to_ffi(error),
            }
        })?;
        let runtime = blocking_runtime()
            .map_err(|reason| FfiBlossomDeleteError::RuntimeUnavailable { reason })?;
        runtime.block_on(async {
            let client = BlossomClient::new(self.config.clone()).map_err(|error| {
                FfiBlossomDeleteError::ClientBuild {
                    reason: error.reason,
                }
            })?;
            client
                .delete(&server, blob, &auth.inner)
                .await
                .map_err(delete_error_to_ffi)
        })
    }

    /// BLOCKING `GET /list/<pubkey>` (BUD-12)
    /// (`nmp_blossom::BlossomClient::list`): the blobs `server_url` stores
    /// for `owner_pubkey_hex`, newest first. `auth` is optional -- a server
    /// that requires a `list` authorization answers 401 on an auth-less
    /// call, surfaced as `AuthRejected`. `cursor_sha256_hex`/`limit` are
    /// the BUD-12 pagination parameters, sent only when set. Call off the
    /// main thread.
    pub fn list(
        &self,
        server_url: String,
        owner_pubkey_hex: String,
        cursor_sha256_hex: Option<String>,
        limit: Option<u32>,
        auth: Option<Arc<FfiBlossomAuthorization>>,
    ) -> Result<Vec<FfiBlobDescriptor>, FfiBlossomListError> {
        let server = Self::parse_server(&server_url, |error| {
            FfiBlossomListError::InvalidServerUrl { error }
        })?;
        let owner = PublicKey::from_hex(&owner_pubkey_hex).map_err(|_| {
            FfiBlossomListError::InvalidOwnerPubkey {
                got: owner_pubkey_hex.clone(),
            }
        })?;
        let cursor = cursor_sha256_hex
            .as_deref()
            .map(Sha256Hash::from_hex)
            .transpose()
            .map_err(|error| FfiBlossomListError::InvalidCursor {
                error: sha256_hex_error_to_ffi(error),
            })?;
        let page = ListPage { cursor, limit };
        let runtime = blocking_runtime()
            .map_err(|reason| FfiBlossomListError::RuntimeUnavailable { reason })?;
        runtime.block_on(async {
            let client = BlossomClient::new(self.config.clone()).map_err(|error| {
                FfiBlossomListError::ClientBuild {
                    reason: error.reason,
                }
            })?;
            let descriptors = client
                .list(&server, owner, &page, auth.as_ref().map(|a| &a.inner))
                .await
                .map_err(list_error_to_ffi)?;
            Ok(descriptors.into_iter().map(descriptor_to_ffi).collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{Alphabet, Kind, TagKind};

    fn hex_of(bytes: &[u8]) -> String {
        Sha256Hash::of(bytes).to_hex()
    }

    /// A signed authorization for `verb` over `blob`, built through the
    /// production draft builders and test keys (this module never signs).
    fn signed_auth_json(
        keys: &nostr::Keys,
        verb: FfiBlossomVerb,
        blob_hex: Option<&str>,
        now: u64,
    ) -> String {
        let draft = match verb {
            FfiBlossomVerb::Upload => blossom_upload_authorization_draft(
                keys.public_key().to_hex(),
                blob_hex.expect("upload binds a blob").to_string(),
                now - 5,
                now + 600,
                "test upload".to_string(),
            ),
            FfiBlossomVerb::Delete => blossom_delete_authorization_draft(
                keys.public_key().to_hex(),
                blob_hex.expect("delete binds a blob").to_string(),
                now - 5,
                now + 600,
                "test delete".to_string(),
            ),
            FfiBlossomVerb::List => blossom_list_authorization_draft(
                keys.public_key().to_hex(),
                now - 5,
                now + 600,
                "test list".to_string(),
            ),
            FfiBlossomVerb::Get => panic!("no get draft builder exists yet (epic #216)"),
        }
        .expect("a valid draft");
        let unsigned =
            UnsignedEvent::from_json(&draft.unsigned_event_json).expect("draft JSON parses");
        let event = unsigned.sign_with_keys(keys).expect("test signing");
        event.as_json()
    }

    /// Invariant (#555): the verb mapping is total and round-trips exactly,
    /// in BOTH directions, with wildcard-free matches on both sides.
    #[test]
    fn verb_mapping_round_trips_totally_in_both_directions() {
        for (rust, ffi) in [
            (BlossomVerb::Upload, FfiBlossomVerb::Upload),
            (BlossomVerb::Delete, FfiBlossomVerb::Delete),
            (BlossomVerb::Get, FfiBlossomVerb::Get),
            (BlossomVerb::List, FfiBlossomVerb::List),
        ] {
            assert_eq!(verb_to_ffi(rust), ffi);
            assert_eq!(verb_from_ffi(ffi), rust);
        }
    }

    #[test]
    fn bud03_demand_and_decode_preserve_order_and_malformed_evidence() {
        let demand = blossom_server_list_demand();
        assert_eq!(demand.selection.kinds, Some(vec![10063]));

        let row = FfiRow {
            id: "11".repeat(32),
            pubkey: "22".repeat(32),
            created_at: 10,
            kind: 10063,
            tags: vec![
                vec!["server".to_string(), "https://first.example".to_string()],
                vec!["server".to_string()],
                vec!["server".to_string(), "ftp://invalid.example".to_string()],
                vec!["server".to_string(), "https://second.example/".to_string()],
            ],
            content: "not-used".to_string(),
            sig: "33".repeat(64),
            sources: vec!["wss://relay.example".to_string()],
        };
        let list = decode_blossom_server_list(row);
        assert_eq!(list.event_id, "11".repeat(32));
        assert_eq!(list.author_pubkey, "22".repeat(32));
        assert_eq!(
            list.servers,
            vec![
                "https://first.example/".to_string(),
                "https://second.example/".to_string(),
            ]
        );
        assert_eq!(list.server_tag_count, 4);
        assert_eq!(list.malformed_entries.len(), 2);
        assert_eq!(list.malformed_entries[0].tag_index, 1);
        assert!(matches!(
            list.malformed_entries[0].error,
            FfiBlossomServerListEntryError::MissingUrl
        ));
        assert_eq!(list.malformed_entries[1].tag_index, 2);
        assert!(matches!(
            list.malformed_entries[1].error,
            FfiBlossomServerListEntryError::InvalidUrl {
                error: FfiBlossomServerUrlError::UnsupportedScheme { .. }
            }
        ));
        assert!(list.unexpected_content);
        assert!(!list.spec_compliant);
    }

    #[test]
    fn qualification_keeps_source_order_and_all_refusal_reasons() {
        let signed_list = FfiBlossomServerList {
            event_id: "11".repeat(32),
            author_pubkey: "22".repeat(32),
            servers: vec![
                "http://127.0.0.1:3000/".to_string(),
                "https://8.8.8.8/".to_string(),
            ],
            malformed_entries: vec![],
            server_tag_count: 2,
            unexpected_content: false,
            spec_compliant: true,
        };
        let client = FfiBlossomClient::new(FfiBlossomClientConfig {
            allowed_local_hosts: vec![],
            max_response_bytes: None,
            max_list_response_bytes: None,
            request_deadline_secs: None,
        });
        let evidence = client
            .qualify_server_candidates(
                FfiBlossomServerCandidatePolicy::SignedListThenOperator,
                vec![
                    "ftp://invalid.example".to_string(),
                    "https://1.1.1.1".to_string(),
                ],
                Some(signed_list),
            )
            .expect("qualification machinery");
        assert_eq!(evidence.len(), 4);
        assert_eq!(
            evidence[0].source,
            FfiBlossomServerCandidateSource::SignedList
        );
        assert!(matches!(
            evidence[0].admission,
            FfiBlossomServerAdmission::LocalHostNotAdmitted { .. }
        ));
        assert!(matches!(
            evidence[1].admission,
            FfiBlossomServerAdmission::Admitted { .. }
        ));
        assert_eq!(
            evidence[2].source,
            FfiBlossomServerCandidateSource::OperatorConfig
        );
        assert!(matches!(
            evidence[2].admission,
            FfiBlossomServerAdmission::InvalidUrl {
                error: FfiBlossomServerUrlError::UnsupportedScheme { .. }
            }
        ));
        assert!(matches!(
            evidence[3].admission,
            FfiBlossomServerAdmission::Admitted { .. }
        ));
    }

    /// Invariant (#555): every `AuthValidationError` variant maps to a
    /// DISTINCT `FfiBlossomAuthError` variant -- no two Rust refusals
    /// collapse into one FFI case (wildcard-free by construction: every
    /// variant is spelled here).
    #[test]
    fn every_auth_validation_variant_maps_to_a_distinct_ffi_variant() {
        let blob = Sha256Hash::of(b"blob");
        let now = Timestamp::from(1_700_000_000u64);
        let later = Timestamp::from(1_700_000_600u64);
        let mapped = vec![
            auth_validation_error_to_ffi(AuthValidationError::WrongKind { found: 1 }),
            auth_validation_error_to_ffi(AuthValidationError::BadSignature {
                reason: "bad".to_string(),
            }),
            auth_validation_error_to_ffi(AuthValidationError::MissingVerb),
            auth_validation_error_to_ffi(AuthValidationError::MultipleVerbs),
            auth_validation_error_to_ffi(AuthValidationError::VerbMismatch {
                expected: BlossomVerb::Upload,
                found: "delete".to_string(),
            }),
            auth_validation_error_to_ffi(AuthValidationError::BlobNotBound { expected: blob }),
            auth_validation_error_to_ffi(AuthValidationError::MissingExpiration),
            auth_validation_error_to_ffi(AuthValidationError::Expired {
                expiration: now,
                now: later,
            }),
            auth_validation_error_to_ffi(AuthValidationError::CreatedAtInFuture {
                created_at: later,
                now,
            }),
        ];
        assert_eq!(
            mapped,
            vec![
                FfiBlossomAuthError::WrongKind { found: 1 },
                FfiBlossomAuthError::BadSignature {
                    reason: "bad".to_string(),
                },
                FfiBlossomAuthError::MissingVerb,
                FfiBlossomAuthError::MultipleVerbs,
                FfiBlossomAuthError::VerbMismatch {
                    expected: FfiBlossomVerb::Upload,
                    found: "delete".to_string(),
                },
                FfiBlossomAuthError::BlobNotBound {
                    expected_sha256_hex: blob.to_hex(),
                },
                FfiBlossomAuthError::MissingExpiration,
                FfiBlossomAuthError::Expired {
                    expiration_secs: now.as_secs(),
                    now_secs: later.as_secs(),
                },
                FfiBlossomAuthError::CreatedAtInFuture {
                    created_at_secs: later.as_secs(),
                    now_secs: now.as_secs(),
                },
            ]
        );
        // Distinctness: no two Rust variants landed on the same FFI variant.
        for (i, a) in mapped.iter().enumerate() {
            for b in mapped.iter().skip(i + 1) {
                assert_ne!(a, b);
            }
        }
        // The draft taxonomy joins the same enum without colliding.
        let draft_err = auth_draft_error_to_ffi(AuthDraftError::ExpirationNotAfterCreatedAt {
            created_at: now,
            expiration: now,
        });
        assert_eq!(
            draft_err,
            FfiBlossomAuthError::ExpirationNotAfterCreatedAt {
                created_at_secs: now.as_secs(),
                expiration_secs: now.as_secs(),
            }
        );
        assert!(!mapped.contains(&draft_err));
    }

    /// Invariant (#555): every `ServerUrlError`, `Sha256HexError`, and
    /// `DescriptorError` variant maps variant-for-variant (wildcard-free:
    /// every variant is spelled here).
    #[test]
    fn server_url_hex_and_descriptor_taxonomies_map_variant_for_variant() {
        assert_eq!(
            server_url_error_to_ffi(ServerUrlError::Parse {
                reason: "r".to_string(),
            }),
            FfiBlossomServerUrlError::Parse {
                reason: "r".to_string(),
            }
        );
        assert_eq!(
            server_url_error_to_ffi(ServerUrlError::MissingHost),
            FfiBlossomServerUrlError::MissingHost
        );
        assert_eq!(
            server_url_error_to_ffi(ServerUrlError::UnsupportedScheme {
                scheme: "ftp".to_string(),
            }),
            FfiBlossomServerUrlError::UnsupportedScheme {
                scheme: "ftp".to_string(),
            }
        );
        assert_eq!(
            server_url_error_to_ffi(ServerUrlError::Credentialed),
            FfiBlossomServerUrlError::Credentialed
        );
        assert_eq!(
            server_url_error_to_ffi(ServerUrlError::NonRootPath {
                path: "/media".to_string(),
            }),
            FfiBlossomServerUrlError::NonRootPath {
                path: "/media".to_string(),
            }
        );
        assert_eq!(
            server_url_error_to_ffi(ServerUrlError::QueryOrFragment),
            FfiBlossomServerUrlError::QueryOrFragment
        );

        assert_eq!(
            sha256_hex_error_to_ffi(Sha256HexError::BadLength { length: 63 }),
            FfiBlossomSha256HexError::BadLength { length: 63 }
        );
        assert_eq!(
            sha256_hex_error_to_ffi(Sha256HexError::NotLowercaseHex { character: 'E' }),
            FfiBlossomSha256HexError::NotLowercaseHex {
                character: "E".to_string(),
            }
        );

        assert_eq!(
            descriptor_error_to_ffi(DescriptorError::TooLarge { limit_bytes: 9 }),
            FfiBlossomDescriptorError::TooLarge { limit_bytes: 9 }
        );
        assert_eq!(
            descriptor_error_to_ffi(DescriptorError::Json {
                reason: "j".to_string(),
            }),
            FfiBlossomDescriptorError::Json {
                reason: "j".to_string(),
            }
        );
        assert_eq!(
            descriptor_error_to_ffi(DescriptorError::MissingUrl),
            FfiBlossomDescriptorError::MissingUrl
        );
        assert_eq!(
            descriptor_error_to_ffi(DescriptorError::MissingSha256),
            FfiBlossomDescriptorError::MissingSha256
        );
        assert_eq!(
            descriptor_error_to_ffi(DescriptorError::MissingSize),
            FfiBlossomDescriptorError::MissingSize
        );
        assert_eq!(
            descriptor_error_to_ffi(DescriptorError::BadSha256(Sha256HexError::BadLength {
                length: 0,
            })),
            FfiBlossomDescriptorError::BadSha256 {
                error: FfiBlossomSha256HexError::BadLength { length: 0 },
            }
        );
    }

    /// Invariant (#555): every `UploadError` variant crosses distinct
    /// (wildcard-free: every variant of the Rust enum is constructed and
    /// mapped here, so a new upstream variant breaks this test at compile
    /// time via the conversion's own exhaustive match).
    #[test]
    fn every_upload_error_variant_crosses_distinct() {
        let blob = Sha256Hash::of(b"a");
        let other = Sha256Hash::of(b"b");
        let all = vec![
            upload_error_to_ffi(UploadError::AuthorizationBlobMismatch {
                expected: blob,
                authorized_verb: BlossomVerb::Delete,
                authorized_blob: Some(other),
            }),
            upload_error_to_ffi(UploadError::LocalHostNotAdmitted {
                host: "localhost".to_string(),
            }),
            upload_error_to_ffi(UploadError::Network {
                detail: "d".to_string(),
            }),
            upload_error_to_ffi(UploadError::RedirectRefused { status: 302 }),
            upload_error_to_ffi(UploadError::AuthRejected {
                status: 401,
                reason: Some("r".to_string()),
            }),
            upload_error_to_ffi(UploadError::ServerRejected {
                status: 413,
                reason: None,
            }),
            upload_error_to_ffi(UploadError::ServerError {
                status: 500,
                reason: None,
            }),
            upload_error_to_ffi(UploadError::ResponseTooLarge { limit_bytes: 65536 }),
            upload_error_to_ffi(UploadError::DescriptorInvalid(DescriptorError::MissingUrl)),
            upload_error_to_ffi(UploadError::Sha256Mismatch {
                expected: blob,
                returned: other,
            }),
        ];
        assert_eq!(
            all[0],
            FfiBlossomUploadError::AuthorizationBlobMismatch {
                expected_sha256_hex: blob.to_hex(),
                authorized_verb: FfiBlossomVerb::Delete,
                authorized_blob_sha256_hex: Some(other.to_hex()),
            }
        );
        assert_eq!(
            all[9],
            FfiBlossomUploadError::Sha256Mismatch {
                expected_sha256_hex: blob.to_hex(),
                returned_sha256_hex: other.to_hex(),
            }
        );
        for (i, a) in all.iter().enumerate() {
            for b in all.iter().skip(i + 1) {
                assert_ne!(a, b);
            }
        }
    }

    /// Invariant (#555): every `MirrorError` variant crosses distinct --
    /// in particular the 409 (`HashMismatchRefused`) and 502
    /// (`OriginFetchFailed`) refusals keep their own FFI variants, and
    /// neither aliases `Sha256Mismatch` or `ServerError`.
    #[test]
    fn every_mirror_error_variant_crosses_distinct() {
        let blob = Sha256Hash::of(b"a");
        let other = Sha256Hash::of(b"b");
        let all = vec![
            mirror_error_to_ffi(MirrorError::AuthorizationBlobMismatch {
                expected: blob,
                authorized_verb: BlossomVerb::List,
                authorized_blob: None,
            }),
            mirror_error_to_ffi(MirrorError::LocalHostNotAdmitted {
                host: "127.0.0.1".to_string(),
            }),
            mirror_error_to_ffi(MirrorError::Network {
                detail: "d".to_string(),
            }),
            mirror_error_to_ffi(MirrorError::RedirectRefused { status: 301 }),
            mirror_error_to_ffi(MirrorError::AuthRejected {
                status: 403,
                reason: None,
            }),
            mirror_error_to_ffi(MirrorError::HashMismatchRefused {
                reason: Some("hash".to_string()),
            }),
            mirror_error_to_ffi(MirrorError::OriginFetchFailed {
                reason: Some("origin".to_string()),
            }),
            mirror_error_to_ffi(MirrorError::ServerRejected {
                status: 400,
                reason: None,
            }),
            mirror_error_to_ffi(MirrorError::ServerError {
                status: 503,
                reason: None,
            }),
            mirror_error_to_ffi(MirrorError::ResponseTooLarge { limit_bytes: 1 }),
            mirror_error_to_ffi(MirrorError::DescriptorInvalid(DescriptorError::MissingSize)),
            mirror_error_to_ffi(MirrorError::Sha256Mismatch {
                expected: blob,
                returned: other,
            }),
        ];
        assert_eq!(
            all[5],
            FfiBlossomMirrorError::HashMismatchRefused {
                reason: Some("hash".to_string()),
            }
        );
        assert_eq!(
            all[6],
            FfiBlossomMirrorError::OriginFetchFailed {
                reason: Some("origin".to_string()),
            }
        );
        for (i, a) in all.iter().enumerate() {
            for b in all.iter().skip(i + 1) {
                assert_ne!(a, b);
            }
        }
    }

    /// Invariant (#555): every `DeleteError` variant crosses distinct --
    /// 404 keeps its own `NotFound` variant (idempotent callers treat
    /// "already gone" as done).
    #[test]
    fn every_delete_error_variant_crosses_distinct() {
        let blob = Sha256Hash::of(b"a");
        let all = [
            delete_error_to_ffi(DeleteError::AuthorizationBlobMismatch {
                expected: blob,
                authorized_verb: BlossomVerb::Upload,
                authorized_blob: Some(blob),
            }),
            delete_error_to_ffi(DeleteError::LocalHostNotAdmitted {
                host: "h".to_string(),
            }),
            delete_error_to_ffi(DeleteError::Network {
                detail: "d".to_string(),
            }),
            delete_error_to_ffi(DeleteError::RedirectRefused { status: 307 }),
            delete_error_to_ffi(DeleteError::AuthRejected {
                status: 402,
                reason: None,
            }),
            delete_error_to_ffi(DeleteError::NotFound {
                reason: Some("gone".to_string()),
            }),
            delete_error_to_ffi(DeleteError::ServerRejected {
                status: 429,
                reason: None,
            }),
            delete_error_to_ffi(DeleteError::ServerError {
                status: 500,
                reason: None,
            }),
        ];
        assert_eq!(
            all[5],
            FfiBlossomDeleteError::NotFound {
                reason: Some("gone".to_string()),
            }
        );
        for (i, a) in all.iter().enumerate() {
            for b in all.iter().skip(i + 1) {
                assert_ne!(a, b);
            }
        }
    }

    /// Invariant (#555): every `ListError` variant crosses distinct -- the
    /// per-row `InvalidDescriptor` refusal keeps its index AND its nested
    /// typed descriptor taxonomy.
    #[test]
    fn every_list_error_variant_crosses_distinct() {
        let all = vec![
            list_error_to_ffi(ListError::WrongVerb {
                authorized_verb: BlossomVerb::Upload,
            }),
            list_error_to_ffi(ListError::LocalHostNotAdmitted {
                host: "h".to_string(),
            }),
            list_error_to_ffi(ListError::Network {
                detail: "d".to_string(),
            }),
            list_error_to_ffi(ListError::RedirectRefused { status: 308 }),
            list_error_to_ffi(ListError::AuthRejected {
                status: 401,
                reason: None,
            }),
            list_error_to_ffi(ListError::ServerRejected {
                status: 400,
                reason: None,
            }),
            list_error_to_ffi(ListError::ServerError {
                status: 502,
                reason: None,
            }),
            list_error_to_ffi(ListError::ResponseTooLarge {
                limit_bytes: 1024 * 1024,
            }),
            list_error_to_ffi(ListError::BodyNotAnArray {
                reason: "object".to_string(),
            }),
            list_error_to_ffi(ListError::InvalidDescriptor {
                index: 3,
                source: DescriptorError::MissingSha256,
            }),
        ];
        assert_eq!(
            all[9],
            FfiBlossomListError::InvalidDescriptor {
                index: 3,
                error: FfiBlossomDescriptorError::MissingSha256,
            }
        );
        for (i, a) in all.iter().enumerate() {
            for b in all.iter().skip(i + 1) {
                assert_ne!(a, b);
            }
        }
    }

    /// Invariant (#555): the descriptor mirror is lossless -- every field
    /// (mandatory and optional, present and absent) survives the crossing.
    #[test]
    fn descriptor_round_trip_is_lossless() {
        let hash = Sha256Hash::of(b"blob");
        let full = BlobDescriptor {
            url: format!("https://cdn.example.com/{}", hash.to_hex()),
            sha256: hash,
            size: 4,
            mime_type: Some("text/plain".to_string()),
            uploaded: Some(1_700_000_000),
        };
        let ffi = descriptor_to_ffi(full.clone());
        assert_eq!(ffi.url, full.url);
        assert_eq!(ffi.sha256, full.sha256.to_hex());
        assert_eq!(ffi.size, full.size);
        assert_eq!(ffi.mime_type, full.mime_type);
        assert_eq!(ffi.uploaded, full.uploaded);

        let minimal = BlobDescriptor {
            url: "https://cdn.example.com/x".to_string(),
            sha256: hash,
            size: 0,
            mime_type: None,
            uploaded: None,
        };
        let ffi = descriptor_to_ffi(minimal);
        assert_eq!(ffi.mime_type, None);
        assert_eq!(ffi.uploaded, None);
    }

    /// Invariant (#555): the draft's canonical JSON parses back to an
    /// `UnsignedEvent` with EXACTLY the crate-composed tags, and the raw
    /// sign-request tokens (`created_at_secs`/`kind`/`tags`/`content`)
    /// agree with that JSON token-for-token -- the two signing paths can
    /// never diverge.
    #[test]
    fn draft_json_and_raw_tokens_agree_on_the_exact_event() {
        let keys = nostr::Keys::generate();
        let blob_hex = hex_of(b"blob");
        let created_at = 1_700_000_000u64;
        let expiration = created_at + 600;
        let draft = blossom_upload_authorization_draft(
            keys.public_key().to_hex(),
            blob_hex.clone(),
            created_at,
            expiration,
            "upload a blob".to_string(),
        )
        .expect("a valid draft");

        assert_eq!(draft.verb, FfiBlossomVerb::Upload);
        assert_eq!(draft.blob_sha256_hex.as_deref(), Some(blob_hex.as_str()));
        assert_eq!(draft.kind, 24242);
        assert_eq!(draft.created_at_secs, created_at);
        assert_eq!(draft.content, "upload a blob");
        assert_eq!(
            draft.tags,
            vec![
                vec!["t".to_string(), "upload".to_string()],
                vec!["x".to_string(), blob_hex.clone()],
                vec!["expiration".to_string(), expiration.to_string()],
            ]
        );

        let unsigned =
            UnsignedEvent::from_json(&draft.unsigned_event_json).expect("draft JSON parses");
        assert_eq!(unsigned.kind, Kind::BlossomAuth);
        assert_eq!(unsigned.pubkey, keys.public_key());
        assert_eq!(unsigned.created_at.as_secs(), created_at);
        assert_eq!(unsigned.content, draft.content);
        let parsed_tags: Vec<Vec<String>> = unsigned
            .tags
            .iter()
            .map(|tag| tag.clone().to_vec())
            .collect();
        assert_eq!(parsed_tags, draft.tags);
        assert_eq!(
            unsigned
                .tags
                .filter(TagKind::single_letter(Alphabet::X, false))
                .count(),
            1
        );

        // The list draft binds no blob and carries no `x` tag.
        let list_draft = blossom_list_authorization_draft(
            keys.public_key().to_hex(),
            created_at,
            expiration,
            "list my blobs".to_string(),
        )
        .expect("a valid list draft");
        assert_eq!(list_draft.blob_sha256_hex, None);
        assert_eq!(list_draft.verb, FfiBlossomVerb::List);
        assert!(!list_draft
            .tags
            .iter()
            .any(|tag| tag.first().map(String::as_str) == Some("x")));
    }

    /// Invariant (#555): boundary-re-introduced draft fallibility is typed
    /// -- a malformed author pubkey, a malformed blob hex, and an
    /// expired-at-birth window each hit their own variant.
    #[test]
    fn draft_input_refusals_are_typed() {
        let keys = nostr::Keys::generate();
        let blob_hex = hex_of(b"blob");
        match blossom_upload_authorization_draft(
            "not-a-pubkey".to_string(),
            blob_hex.clone(),
            1,
            2,
            String::new(),
        ) {
            Err(FfiBlossomAuthError::InvalidAuthorPubkey { got }) => {
                assert_eq!(got, "not-a-pubkey")
            }
            other => panic!("expected InvalidAuthorPubkey, got {other:?}"),
        }
        match blossom_delete_authorization_draft(
            keys.public_key().to_hex(),
            "UPPER".to_string(),
            1,
            2,
            String::new(),
        ) {
            Err(FfiBlossomAuthError::InvalidBlobSha256 {
                error: FfiBlossomSha256HexError::BadLength { length: 5 },
            }) => {}
            other => panic!("expected InvalidBlobSha256/BadLength, got {other:?}"),
        }
        match blossom_upload_authorization_draft(
            keys.public_key().to_hex(),
            blob_hex,
            2,
            2,
            String::new(),
        ) {
            Err(FfiBlossomAuthError::ExpirationNotAfterCreatedAt {
                created_at_secs: 2,
                expiration_secs: 2,
            }) => {}
            other => panic!("expected ExpirationNotAfterCreatedAt, got {other:?}"),
        }
    }

    /// Falsifier (#555, Destructive/Reachability gate): fail-closed BUD-11
    /// validation SURVIVES the FFI boundary -- a well-formed signed
    /// authorization validates, and the same event with one tampered byte
    /// of content is refused as `BadSignature`, never admitted and never a
    /// panic.
    #[test]
    fn validate_through_ffi_admits_the_real_event_and_refuses_a_tampered_one() {
        let keys = nostr::Keys::generate();
        let blob_hex = hex_of(b"blob");
        let now = 1_700_000_000u64;
        let json = signed_auth_json(&keys, FfiBlossomVerb::Upload, Some(&blob_hex), now);

        let auth = FfiBlossomAuthorization::validate(
            json.clone(),
            FfiBlossomVerb::Upload,
            Some(blob_hex.clone()),
            now,
        )
        .expect("a freshly signed authorization validates through FFI");
        assert_eq!(auth.verb(), FfiBlossomVerb::Upload);
        assert_eq!(auth.blob_sha256_hex().as_deref(), Some(blob_hex.as_str()));

        let mut tampered: serde_json::Value =
            serde_json::from_str(&json).expect("event JSON parses");
        tampered["content"] = serde_json::Value::String("tampered".to_string());
        match FfiBlossomAuthorization::validate(
            tampered.to_string(),
            FfiBlossomVerb::Upload,
            Some(blob_hex.clone()),
            now,
        ) {
            Err(FfiBlossomAuthError::BadSignature { .. }) => {}
            Ok(_) => panic!("a tampered authorization must never validate"),
            Err(other) => panic!("expected BadSignature, got {other:?}"),
        }

        // Cross-verb replay is refused typed.
        match FfiBlossomAuthorization::validate(
            json.clone(),
            FfiBlossomVerb::Delete,
            Some(blob_hex.clone()),
            now,
        ) {
            Err(FfiBlossomAuthError::VerbMismatch {
                expected: FfiBlossomVerb::Delete,
                found,
            }) => assert_eq!(found, "upload"),
            other => panic!("expected VerbMismatch, got {other:?}"),
        }

        // Cross-blob replay is refused typed.
        let other_hex = hex_of(b"other blob");
        match FfiBlossomAuthorization::validate(
            json,
            FfiBlossomVerb::Upload,
            Some(other_hex.clone()),
            now,
        ) {
            Err(FfiBlossomAuthError::BlobNotBound {
                expected_sha256_hex,
            }) => assert_eq!(expected_sha256_hex, other_hex),
            other => panic!("expected BlobNotBound, got {other:?}"),
        }

        // Non-JSON input is a typed refusal, never a panic.
        match FfiBlossomAuthorization::validate(
            "not json".to_string(),
            FfiBlossomVerb::Upload,
            None,
            now,
        ) {
            Err(FfiBlossomAuthError::InvalidEventJson { .. }) => {}
            other => panic!("expected InvalidEventJson, got {other:?}"),
        }
    }

    /// Falsifier (#555): the `FfiSignedEvent` door (the engine sign-only
    /// path's exact output shape) runs the SAME fail-closed validation --
    /// the genuine event validates, one tampered field is `BadSignature`.
    #[test]
    fn validate_signed_event_record_admits_real_and_refuses_tampered() {
        let keys = nostr::Keys::generate();
        let blob_hex = hex_of(b"blob");
        let now = 1_700_000_000u64;
        let json = signed_auth_json(&keys, FfiBlossomVerb::Delete, Some(&blob_hex), now);
        let event = nostr::Event::from_json(&json).expect("event JSON parses");
        let record = FfiSignedEvent {
            id: event.id.to_hex(),
            pubkey: event.pubkey.to_hex(),
            created_at: event.created_at.as_secs(),
            kind: event.kind.as_u16(),
            tags: event.tags.iter().map(|tag| tag.clone().to_vec()).collect(),
            content: event.content.clone(),
            sig: event.sig.to_string(),
        };

        let auth = FfiBlossomAuthorization::validate_signed_event(
            record.clone(),
            FfiBlossomVerb::Delete,
            Some(blob_hex.clone()),
            now,
        )
        .expect("the engine-signed record validates through FFI");
        assert_eq!(auth.verb(), FfiBlossomVerb::Delete);

        let mut tampered = record.clone();
        tampered.content = "tampered".to_string();
        match FfiBlossomAuthorization::validate_signed_event(
            tampered,
            FfiBlossomVerb::Delete,
            Some(blob_hex.clone()),
            now,
        ) {
            Err(FfiBlossomAuthError::BadSignature { .. }) => {}
            Ok(_) => panic!("a tampered record must never validate"),
            Err(other) => panic!("expected BadSignature, got {other:?}"),
        }

        let mut malformed = record;
        malformed.sig = "zz".to_string();
        match FfiBlossomAuthorization::validate_signed_event(
            malformed,
            FfiBlossomVerb::Delete,
            Some(blob_hex),
            now,
        ) {
            Err(FfiBlossomAuthError::InvalidEventJson { .. }) => {}
            other => panic!("expected InvalidEventJson, got {other:?}"),
        }
    }

    /// Invariant (#555): `None` config knobs resolve to EXACTLY the crate
    /// defaults, and explicit values pass through.
    #[test]
    fn client_config_nones_resolve_to_crate_defaults() {
        let defaults = client_config_from_ffi(FfiBlossomClientConfig {
            allowed_local_hosts: vec![],
            max_response_bytes: None,
            max_list_response_bytes: None,
            request_deadline_secs: None,
        });
        assert!(defaults.allowed_local_hosts.is_empty());
        assert_eq!(defaults.max_response_bytes, DEFAULT_MAX_RESPONSE_BYTES);
        assert_eq!(
            defaults.max_list_response_bytes,
            DEFAULT_MAX_LIST_RESPONSE_BYTES
        );
        assert_eq!(defaults.request_deadline, DEFAULT_REQUEST_DEADLINE);

        let explicit = client_config_from_ffi(FfiBlossomClientConfig {
            allowed_local_hosts: vec!["localhost".to_string(), "localhost".to_string()],
            max_response_bytes: Some(1024),
            max_list_response_bytes: Some(2048),
            request_deadline_secs: Some(5),
        });
        assert_eq!(explicit.allowed_local_hosts.len(), 1, "hosts deduplicate");
        assert_eq!(explicit.max_response_bytes, 1024);
        assert_eq!(explicit.max_list_response_bytes, 2048);
        assert_eq!(explicit.request_deadline, Duration::from_secs(5));
    }

    /// Invariant (#555): boundary-re-introduced client fallibility is typed
    /// -- a malformed server URL is refused BEFORE any runtime or socket
    /// exists, carrying the exact `ServerUrlError` mirror.
    #[test]
    fn client_operations_refuse_malformed_inputs_typed_before_any_io() {
        let keys = nostr::Keys::generate();
        let blob_hex = hex_of(b"blob");
        let now = 1_700_000_000u64;
        let auth = FfiBlossomAuthorization::validate(
            signed_auth_json(&keys, FfiBlossomVerb::Upload, Some(&blob_hex), now),
            FfiBlossomVerb::Upload,
            Some(blob_hex.clone()),
            now,
        )
        .expect("a valid authorization");
        let client = FfiBlossomClient::new(FfiBlossomClientConfig {
            allowed_local_hosts: vec![],
            max_response_bytes: None,
            max_list_response_bytes: None,
            request_deadline_secs: None,
        });

        match client.upload(
            "ftp://cdn.example.com".to_string(),
            b"blob".to_vec(),
            None,
            Arc::clone(&auth),
        ) {
            Err(FfiBlossomUploadError::InvalidServerUrl {
                error: FfiBlossomServerUrlError::UnsupportedScheme { scheme },
            }) => assert_eq!(scheme, "ftp"),
            other => panic!("expected InvalidServerUrl/UnsupportedScheme, got {other:?}"),
        }

        match client.delete(
            "https://cdn.example.com".to_string(),
            "not-hex".to_string(),
            Arc::clone(&auth),
        ) {
            Err(FfiBlossomDeleteError::InvalidBlobSha256 { .. }) => {}
            other => panic!("expected InvalidBlobSha256, got {other:?}"),
        }

        match client.mirror(
            "https://cdn.example.com".to_string(),
            "https://origin.example.com/blob".to_string(),
            "short".to_string(),
            Arc::clone(&auth),
        ) {
            Err(FfiBlossomMirrorError::InvalidExpectedSha256 { .. }) => {}
            other => panic!("expected InvalidExpectedSha256, got {other:?}"),
        }

        match client.list(
            "https://cdn.example.com".to_string(),
            "nope".to_string(),
            None,
            None,
            None,
        ) {
            Err(FfiBlossomListError::InvalidOwnerPubkey { got }) => assert_eq!(got, "nope"),
            other => panic!("expected InvalidOwnerPubkey, got {other:?}"),
        }

        match client.list(
            "https://cdn.example.com".to_string(),
            keys.public_key().to_hex(),
            Some("bad-cursor".to_string()),
            Some(10),
            None,
        ) {
            Err(FfiBlossomListError::InvalidCursor { .. }) => {}
            other => panic!("expected InvalidCursor, got {other:?}"),
        }
    }

    /// Invariant (#555, Reachability Gate, adversarial-review finding):
    /// the boundary-machinery variants with no Rust-core counterpart are
    /// constructible and distinct on EVERY operation taxonomy --
    /// `InvalidServerUrl` reaches mirror/delete/list through the real
    /// operations (upload's is proven above), and
    /// `RuntimeUnavailable`/`ClientBuild`, which only real runtime or
    /// HTTP-stack construction failure can produce in production, are
    /// pinned directly as typed slots distinct from every wire failure.
    #[test]
    fn boundary_machinery_variants_are_constructible_and_distinct() {
        let keys = nostr::Keys::generate();
        let blob_hex = hex_of(b"blob");
        let now = 1_700_000_000u64;
        let auth = FfiBlossomAuthorization::validate(
            signed_auth_json(&keys, FfiBlossomVerb::Upload, Some(&blob_hex), now),
            FfiBlossomVerb::Upload,
            Some(blob_hex.clone()),
            now,
        )
        .expect("a valid authorization");
        let client = FfiBlossomClient::new(FfiBlossomClientConfig {
            allowed_local_hosts: vec![],
            max_response_bytes: None,
            max_list_response_bytes: None,
            request_deadline_secs: None,
        });

        match client.mirror(
            "ftp://cdn.example.com".to_string(),
            "https://origin.example.com/blob".to_string(),
            blob_hex.clone(),
            Arc::clone(&auth),
        ) {
            Err(FfiBlossomMirrorError::InvalidServerUrl {
                error: FfiBlossomServerUrlError::UnsupportedScheme { scheme },
            }) => assert_eq!(scheme, "ftp"),
            other => panic!("expected mirror InvalidServerUrl, got {other:?}"),
        }
        match client.delete(
            "ftp://cdn.example.com".to_string(),
            blob_hex.clone(),
            Arc::clone(&auth),
        ) {
            Err(FfiBlossomDeleteError::InvalidServerUrl {
                error: FfiBlossomServerUrlError::UnsupportedScheme { scheme },
            }) => assert_eq!(scheme, "ftp"),
            other => panic!("expected delete InvalidServerUrl, got {other:?}"),
        }
        match client.list(
            "ftp://cdn.example.com".to_string(),
            keys.public_key().to_hex(),
            None,
            None,
            None,
        ) {
            Err(FfiBlossomListError::InvalidServerUrl {
                error: FfiBlossomServerUrlError::UnsupportedScheme { scheme },
            }) => assert_eq!(scheme, "ftp"),
            other => panic!("expected list InvalidServerUrl, got {other:?}"),
        }

        let reason = "machinery".to_string();
        let upload_machinery = [
            FfiBlossomUploadError::RuntimeUnavailable {
                reason: reason.clone(),
            },
            FfiBlossomUploadError::ClientBuild {
                reason: reason.clone(),
            },
            FfiBlossomUploadError::Network {
                detail: reason.clone(),
            },
        ];
        let mirror_machinery = [
            FfiBlossomMirrorError::RuntimeUnavailable {
                reason: reason.clone(),
            },
            FfiBlossomMirrorError::ClientBuild {
                reason: reason.clone(),
            },
            FfiBlossomMirrorError::Network {
                detail: reason.clone(),
            },
        ];
        let delete_machinery = [
            FfiBlossomDeleteError::RuntimeUnavailable {
                reason: reason.clone(),
            },
            FfiBlossomDeleteError::ClientBuild {
                reason: reason.clone(),
            },
            FfiBlossomDeleteError::Network {
                detail: reason.clone(),
            },
        ];
        let list_machinery = [
            FfiBlossomListError::RuntimeUnavailable {
                reason: reason.clone(),
            },
            FfiBlossomListError::ClientBuild {
                reason: reason.clone(),
            },
            FfiBlossomListError::Network { detail: reason },
        ];
        for (i, a) in upload_machinery.iter().enumerate() {
            for b in upload_machinery.iter().skip(i + 1) {
                assert_ne!(a, b);
            }
        }
        for (i, a) in mirror_machinery.iter().enumerate() {
            for b in mirror_machinery.iter().skip(i + 1) {
                assert_ne!(a, b);
            }
        }
        for (i, a) in delete_machinery.iter().enumerate() {
            for b in delete_machinery.iter().skip(i + 1) {
                assert_ne!(a, b);
            }
        }
        for (i, a) in list_machinery.iter().enumerate() {
            for b in list_machinery.iter().skip(i + 1) {
                assert_ne!(a, b);
            }
        }
    }

    /// Falsifier (#555, Destructive gate): the pre-I/O admission and
    /// authorization-binding refusals survive the boundary through a REAL
    /// blocking client call -- an unadmitted loopback host dies typed
    /// before any socket I/O, and a cross-verb authorization dies typed
    /// before admission.
    #[test]
    fn blocking_client_call_carries_typed_refusals_across_the_boundary() {
        let keys = nostr::Keys::generate();
        let blob = b"boundary blob".to_vec();
        let blob_hex = hex_of(&blob);
        let now = nostr::Timestamp::now().as_secs();
        let upload_auth = FfiBlossomAuthorization::validate(
            signed_auth_json(&keys, FfiBlossomVerb::Upload, Some(&blob_hex), now),
            FfiBlossomVerb::Upload,
            Some(blob_hex.clone()),
            now,
        )
        .expect("a valid upload authorization");
        let client = FfiBlossomClient::new(FfiBlossomClientConfig {
            allowed_local_hosts: vec![],
            max_response_bytes: None,
            max_list_response_bytes: None,
            request_deadline_secs: Some(5),
        });

        // Unadmitted loopback host: refused before ANY socket I/O.
        match client.upload(
            "http://127.0.0.1:1".to_string(),
            blob.clone(),
            None,
            Arc::clone(&upload_auth),
        ) {
            Err(FfiBlossomUploadError::LocalHostNotAdmitted { host }) => {
                assert_eq!(host, "127.0.0.1")
            }
            other => panic!("expected LocalHostNotAdmitted, got {other:?}"),
        }

        // Cross-verb replay through the client: an upload grant does not
        // delete, refused before admission or I/O.
        match client.delete(
            "https://cdn.example.com".to_string(),
            blob_hex.clone(),
            upload_auth,
        ) {
            Err(FfiBlossomDeleteError::AuthorizationBlobMismatch {
                expected_sha256_hex,
                authorized_verb: FfiBlossomVerb::Upload,
                authorized_blob_sha256_hex,
            }) => {
                assert_eq!(expected_sha256_hex, blob_hex);
                assert_eq!(
                    authorized_blob_sha256_hex.as_deref(),
                    Some(blob_hex.as_str())
                );
            }
            other => panic!("expected AuthorizationBlobMismatch, got {other:?}"),
        }
    }
}
