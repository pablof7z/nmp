//! BUD-02/04/12 blob client (#545, #551): async, self-verifying `PUT
//! /upload`, `PUT /mirror`, `DELETE /<sha256>`, and `GET /list/<pubkey>`
//! with redirects/proxies/referrers/retries disabled and every response
//! body streamed under a byte cap. Each operation validates its authorization
//! binding before performing I/O; each has its own exhaustive error enum
//! (operation failures are never collapsed into one taxonomy).

use std::time::Duration;

use nmp_asset::{Sha256Hash, VerifiedAsset};

use crate::auth::{BlossomVerb, SignedAuthorization};
use crate::descriptor::{BlobDescriptor, DescriptorError};

/// Default cap on a blob-descriptor response body.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 65536;

/// Default overall request deadline (connect, headers, and body).
pub const DEFAULT_REQUEST_DEADLINE: Duration = Duration::from_secs(60);

/// Default cap on a `GET /list` response body (#551). A list is an ARRAY
/// of descriptors, so it legitimately exceeds the single-descriptor cap
/// ([`DEFAULT_MAX_RESPONSE_BYTES`]); 1 MiB bounds roughly four thousand
/// full descriptors while still refusing a hostile unbounded body.
pub const DEFAULT_MAX_LIST_RESPONSE_BYTES: usize = 1024 * 1024;

/// A validated Blossom server base URL: http/https, host present, root
/// path, no query/fragment, no credentials. BUD endpoints live at the
/// domain root (`PUT /upload`), so anything else is a typed refusal
/// rather than a silent rewrite (Destructive-API discipline: this type
/// never normalizes input, it refuses it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlossomServerUrl {
    url: reqwest::Url,
}

/// [`BlossomServerUrl::parse`]'s failure modes. Exhaustive; each variant
/// is pinned by the unit tests below. Checks run host-first so a hostless
/// URL (`mailto:...`) is a `MissingHost` refusal even before its scheme is
/// judged -- both orders fail closed, this one keeps every variant
/// honestly constructible (the url crate guarantees http/https URLs always
/// have a host, so a scheme-first order would make `MissingHost` dead).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerUrlError {
    /// Not a parseable URL at all.
    Parse { reason: String },
    /// The URL names no host.
    MissingHost,
    /// A scheme other than `http`/`https`.
    UnsupportedScheme { scheme: String },
    /// URL userinfo would become an ambient `Authorization`-adjacent
    /// credential; refused outright (same rule as the engine's
    /// `CredentialedRelayUrl`).
    Credentialed,
    /// BUD endpoints live at the domain root; a base URL with a path would
    /// silently change every endpoint this client derives.
    NonRootPath { path: String },
    /// A query or fragment would survive into derived endpoint URLs;
    /// refused rather than stripped.
    QueryOrFragment,
}

impl std::fmt::Display for ServerUrlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse { reason } => write!(f, "Blossom server URL does not parse: {reason}"),
            Self::MissingHost => f.write_str("Blossom server URL has no host"),
            Self::UnsupportedScheme { scheme } => {
                write!(f, "Blossom server URL scheme {scheme:?} is not http/https")
            }
            Self::Credentialed => f.write_str("Blossom server URL carries credentials"),
            Self::NonRootPath { path } => {
                write!(f, "Blossom server URL path {path:?} is not the domain root")
            }
            Self::QueryOrFragment => f.write_str("Blossom server URL carries a query or fragment"),
        }
    }
}

impl std::error::Error for ServerUrlError {}

impl BlossomServerUrl {
    /// Validate a Blossom server base URL. See the type doc for the exact
    /// admission rules; nothing is ever normalized away.
    pub fn parse(input: &str) -> Result<Self, ServerUrlError> {
        let url = reqwest::Url::parse(input).map_err(|error| ServerUrlError::Parse {
            reason: error.to_string(),
        })?;
        url.host_str().ok_or(ServerUrlError::MissingHost)?;
        let scheme = url.scheme();
        if scheme != "http" && scheme != "https" {
            return Err(ServerUrlError::UnsupportedScheme {
                scheme: scheme.to_string(),
            });
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(ServerUrlError::Credentialed);
        }
        if !matches!(url.path(), "" | "/") {
            return Err(ServerUrlError::NonRootPath {
                path: url.path().to_string(),
            });
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(ServerUrlError::QueryOrFragment);
        }
        Ok(Self { url })
    }

    /// The validated base URL text.
    pub fn as_str(&self) -> &str {
        self.url.as_str()
    }

    /// The BUD-02 upload endpoint at this server's root.
    fn upload_endpoint(&self) -> reqwest::Url {
        let mut url = self.url.clone();
        url.set_path("/upload");
        url
    }

    /// The BUD-04 mirror endpoint at this server's root.
    fn mirror_endpoint(&self) -> reqwest::Url {
        let mut url = self.url.clone();
        url.set_path("/mirror");
        url
    }

    /// The BUD-01/12 per-blob endpoint (`/<lowercase-hex sha256>`) at this
    /// server's root -- the DELETE target.
    fn blob_endpoint(&self, blob: Sha256Hash) -> reqwest::Url {
        let mut url = self.url.clone();
        url.set_path(&format!("/{}", blob.to_hex()));
        url
    }

    /// The BUD-12 list endpoint (`/list/<pubkey-hex>`) with `cursor`/
    /// `limit` query parameters appended ONLY when set (`since`/`until`
    /// are deprecated upstream and deliberately not modeled). The query
    /// serializer is only entered when at least one parameter is present,
    /// so a parameterless page derives a clean `?`-free URL.
    fn list_endpoint(&self, owner: &nostr::PublicKey, page: &ListPage) -> reqwest::Url {
        let mut url = self.url.clone();
        url.set_path(&format!("/list/{}", owner.to_hex()));
        if page.cursor.is_some() || page.limit.is_some() {
            let mut pairs = url.query_pairs_mut();
            if let Some(cursor) = page.cursor {
                pairs.append_pair("cursor", &cursor.to_hex());
            }
            if let Some(limit) = page.limit {
                pairs.append_pair("limit", &limit.to_string());
            }
        }
        url
    }
}

/// [`BlossomClient`] construction knobs.
#[derive(Debug, Clone)]
pub struct BlossomClientConfig {
    /// Cap on a single-descriptor response body (upload/mirror), enforced
    /// while streaming.
    pub max_response_bytes: usize,
    /// Cap on a `GET /list` response body (#551), enforced while
    /// streaming. Separate from `max_response_bytes` because a list is an
    /// array of descriptors and legitimately larger than one.
    pub max_list_response_bytes: usize,
    /// Overall request deadline (connect, headers, and body).
    pub request_deadline: Duration,
}

impl Default for BlossomClientConfig {
    fn default() -> Self {
        Self {
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            max_list_response_bytes: DEFAULT_MAX_LIST_RESPONSE_BYTES,
            request_deadline: DEFAULT_REQUEST_DEADLINE,
        }
    }
}

/// [`BlossomClient::new`]'s failure: the HTTP stack could not be
/// constructed (system DNS configuration unreadable, or reqwest client
/// construction failed). A struct rather than an enum: both causes are
/// unrecoverable-at-this-layer construction failures a caller handles
/// identically, and neither is reachable on a healthy host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientBuildError {
    pub reason: String,
}

impl std::fmt::Display for ClientBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Blossom HTTP client construction failed: {}",
            self.reason
        )
    }
}

impl std::error::Error for ClientBuildError {}

/// [`BlossomClient::upload`]'s exhaustive, separated failure taxonomy
/// (#545). Every variant is constructed by the client and pinned by
/// `tests/upload_contract.rs`; the taxonomy test proves exhaustiveness
/// with a wildcard-free `match`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadError {
    /// The supplied authorization is not an `upload` grant for EXACTLY the
    /// blob bytes being uploaded -- refused before any admission or I/O,
    /// so an authorization can never be replayed across blobs or verbs.
    AuthorizationBlobMismatch {
        expected: Sha256Hash,
        authorized_verb: BlossomVerb,
        authorized_blob: Option<Sha256Hash>,
    },
    /// Transport failure: connect/DNS/TLS/timeout, or the body stream died.
    Network { detail: String },
    /// The server answered with a redirect; redirects are never followed
    /// (an upload must not be silently re-aimed at another authority).
    RedirectRefused { status: u16 },
    /// 401/403: the server refused the authorization itself.
    AuthRejected { status: u16, reason: Option<String> },
    /// Any other non-success, non-5xx status (413, 415, 404, ...): the
    /// server refused this request without indicting the authorization.
    ServerRejected { status: u16, reason: Option<String> },
    /// 5xx: the server failed.
    ServerError { status: u16, reason: Option<String> },
    /// The success body exceeded the configured response cap.
    ResponseTooLarge { limit_bytes: usize },
    /// The success body is not a valid BUD-02 blob descriptor.
    DescriptorInvalid(DescriptorError),
    /// INTEGRITY GATE: the server's descriptor names a different sha256
    /// than the bytes this client just hashed and sent -- fail closed, the
    /// descriptor is never returned.
    Sha256Mismatch {
        expected: Sha256Hash,
        returned: Sha256Hash,
    },
}

impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AuthorizationBlobMismatch {
                expected,
                authorized_verb,
                authorized_blob,
            } => write!(
                f,
                "authorization ({authorized_verb}, blob {:?}) does not grant uploading blob {}",
                authorized_blob.map(|hash| hash.to_hex()),
                expected.to_hex()
            ),
            Self::Network { detail } => write!(f, "Blossom upload transport failed: {detail}"),
            Self::RedirectRefused { status } => {
                write!(f, "Blossom upload redirects are not followed (HTTP {status})")
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
            Self::DescriptorInvalid(error) => write!(f, "Blossom descriptor invalid: {error}"),
            Self::Sha256Mismatch { expected, returned } => write!(
                f,
                "Blossom server returned sha256 {} for a blob hashing to {} -- refusing the descriptor",
                returned.to_hex(),
                expected.to_hex()
            ),
        }
    }
}

impl std::error::Error for UploadError {}

/// The result of an upload whose bytes THIS client observed: the server's
/// integrity-checked BUD-02 descriptor plus the protocol-neutral
/// [`VerifiedAsset`] proof computed from those exact bytes (#884).
///
/// Private fields + no public constructor, and only [`BlossomClient::upload`]
/// builds one. [`BlossomClient::mirror`] deliberately cannot: a mirroring
/// client never sees the mirrored bytes, so it has nothing from which to
/// mint an exact-byte witness (type-over-convention -- the impossibility is
/// structural, not a review note).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedUpload {
    descriptor: BlobDescriptor,
    asset: VerifiedAsset,
}

impl VerifiedUpload {
    /// The server's descriptor, integrity-checked against the uploaded
    /// bytes. Still the SERVER's claim vocabulary -- `url`/`mime_type` are
    /// untrusted text.
    pub fn descriptor(&self) -> &BlobDescriptor {
        &self.descriptor
    }

    /// The exact-byte proof computed locally from the uploaded bytes,
    /// independent of Blossom.
    pub fn asset(&self) -> &VerifiedAsset {
        &self.asset
    }

    /// Consume into the verified descriptor.
    pub fn into_descriptor(self) -> BlobDescriptor {
        self.descriptor
    }

    /// Consume into the protocol-neutral exact-byte proof.
    pub fn into_asset(self) -> VerifiedAsset {
        self.asset
    }
}

/// [`BlossomClient::mirror`]'s exhaustive, separated failure taxonomy
/// (#551, BUD-04). Every variant is constructed by
/// `tests/mirror_delete_list_contract.rs`, whose wildcard-free taxonomy
/// `match` proves exhaustiveness. Deliberately its own enum rather than a
/// reuse of [`UploadError`]: mirror has failure modes with no upload
/// analogue (the server's 409 hash refusal, the 502 origin-fetch failure)
/// and operation failures are never collapsed across operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MirrorError {
    /// The supplied authorization is not an `upload` grant (BUD-04
    /// authorizes mirroring with the `upload` verb) for EXACTLY the
    /// expected blob hash -- refused before any admission or I/O.
    AuthorizationBlobMismatch {
        expected: Sha256Hash,
        authorized_verb: BlossomVerb,
        authorized_blob: Option<Sha256Hash>,
    },
    /// Transport failure: connect/DNS/TLS/timeout, or the body stream died.
    Network { detail: String },
    /// The server answered with a redirect; redirects are never followed
    /// (a mirror must not be silently re-aimed at another authority).
    RedirectRefused { status: u16 },
    /// 401/403: the server refused the authorization itself.
    AuthRejected { status: u16, reason: Option<String> },
    /// 409: the SERVER downloaded the source URL and found its hash does
    /// not match the authorized `x` tag (BUD-04) -- the server's refusal.
    /// Distinct from [`Self::Sha256Mismatch`], which is THIS CLIENT
    /// refusing a 2xx descriptor whose sha256 differs from `expected`.
    HashMismatchRefused { reason: Option<String> },
    /// 502: the destination server could not fetch the source URL
    /// (BUD-04) -- the origin failed, not the destination, so this is
    /// distinct from [`Self::ServerError`] and matched BEFORE the generic
    /// 5xx arm.
    OriginFetchFailed { reason: Option<String> },
    /// Any other non-success, non-5xx status (400, 413, 415, ...): the
    /// server refused this request without indicting the authorization.
    ServerRejected { status: u16, reason: Option<String> },
    /// Any 5xx other than 502: the destination server itself failed.
    ServerError { status: u16, reason: Option<String> },
    /// The success body exceeded the configured response cap.
    ResponseTooLarge { limit_bytes: usize },
    /// The success body is not a valid BUD-02 blob descriptor.
    DescriptorInvalid(DescriptorError),
    /// INTEGRITY GATE: the 2xx descriptor names a different sha256 than
    /// the hash this client authorized -- fail closed, no descriptor
    /// escapes.
    Sha256Mismatch {
        expected: Sha256Hash,
        returned: Sha256Hash,
    },
}

impl std::fmt::Display for MirrorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AuthorizationBlobMismatch {
                expected,
                authorized_verb,
                authorized_blob,
            } => write!(
                f,
                "authorization ({authorized_verb}, blob {:?}) does not grant mirroring blob {}",
                authorized_blob.map(|hash| hash.to_hex()),
                expected.to_hex()
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
            Self::DescriptorInvalid(error) => write!(f, "Blossom descriptor invalid: {error}"),
            Self::Sha256Mismatch { expected, returned } => write!(
                f,
                "Blossom server returned sha256 {} for a mirror authorized as {} -- refusing \
                 the descriptor",
                returned.to_hex(),
                expected.to_hex()
            ),
        }
    }
}

impl std::error::Error for MirrorError {}

/// [`BlossomClient::delete`]'s exhaustive, separated failure taxonomy
/// (#551, BUD-12). Every variant is constructed by
/// `tests/mirror_delete_list_contract.rs`, whose wildcard-free taxonomy
/// `match` proves exhaustiveness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteError {
    /// The supplied authorization is not a `delete` grant for EXACTLY the
    /// blob named in the request path -- refused before any admission or
    /// I/O. BUD-12: multiple `x` tags MUST NOT widen a delete to multiple
    /// blobs; [`SignedAuthorization`] witnesses exactly ONE expected hash,
    /// and this check compares that witness to the path hash.
    AuthorizationBlobMismatch {
        expected: Sha256Hash,
        authorized_verb: BlossomVerb,
        authorized_blob: Option<Sha256Hash>,
    },
    /// Transport failure: connect/DNS/TLS/timeout, or the body stream died.
    Network { detail: String },
    /// The server answered with a redirect; redirects are never followed
    /// (a delete must not be silently re-aimed at another authority).
    RedirectRefused { status: u16 },
    /// 401/402/403: the server refused the authorization (BUD-12 names
    /// all three as authorization-indicting for DELETE).
    AuthRejected { status: u16, reason: Option<String> },
    /// 404: no blob with this sha256 exists on the server -- its own
    /// variant because "already gone" is actionable (idempotent callers
    /// treat it as done) where a generic rejection is not.
    NotFound { reason: Option<String> },
    /// Any other non-success, non-5xx status (429, ...): the server
    /// refused this request without indicting the authorization.
    ServerRejected { status: u16, reason: Option<String> },
    /// 5xx: the server failed.
    ServerError { status: u16, reason: Option<String> },
}

impl std::fmt::Display for DeleteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AuthorizationBlobMismatch {
                expected,
                authorized_verb,
                authorized_blob,
            } => write!(
                f,
                "authorization ({authorized_verb}, blob {:?}) does not grant deleting blob {}",
                authorized_blob.map(|hash| hash.to_hex()),
                expected.to_hex()
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

impl std::error::Error for DeleteError {}

/// BUD-12 `GET /list` pagination: `cursor` is the sha256 of the last blob
/// of the previous page, `limit` the page size. `None` fields are simply
/// not sent (the deprecated `since`/`until` parameters are deliberately
/// not modeled). [`Default`] is the parameterless first page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ListPage {
    pub cursor: Option<Sha256Hash>,
    pub limit: Option<u32>,
}

/// [`BlossomClient::list`]'s exhaustive, separated failure taxonomy
/// (#551, BUD-12). Every variant is constructed by
/// `tests/mirror_delete_list_contract.rs`, whose wildcard-free taxonomy
/// `match` proves exhaustiveness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListError {
    /// An authorization was supplied but is not a `list` grant -- refused
    /// before any admission or I/O (no cross-verb replay).
    WrongVerb { authorized_verb: BlossomVerb },
    /// Transport failure: connect/DNS/TLS/timeout, or the body stream died.
    Network { detail: String },
    /// The server answered with a redirect; redirects are never followed
    /// (a list must not be silently re-aimed at another authority).
    RedirectRefused { status: u16 },
    /// 401/402/403: the server refused the request as unauthorized --
    /// including a 401 on an auth-less call to a server that requires a
    /// `list` authorization (BUD-12 allows servers to require one).
    AuthRejected { status: u16, reason: Option<String> },
    /// Any other non-success, non-5xx status: the server refused this
    /// request without indicting the authorization.
    ServerRejected { status: u16, reason: Option<String> },
    /// 5xx: the server failed.
    ServerError { status: u16, reason: Option<String> },
    /// The success body exceeded the configured list response cap.
    ResponseTooLarge { limit_bytes: usize },
    /// The success body is not a top-level JSON array -- refused outright,
    /// never coerced.
    BodyNotAnArray { reason: String },
    /// Array element `index` failed the strict BUD-02 descriptor rules --
    /// the WHOLE list fails typed; a malformed row is never silently
    /// skipped and the well-formed prefix is never returned as a
    /// truncated success.
    InvalidDescriptor {
        index: usize,
        source: DescriptorError,
    },
}

impl std::fmt::Display for ListError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongVerb { authorized_verb } => write!(
                f,
                "authorization verb {authorized_verb} does not grant listing (need `list`)"
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
            Self::InvalidDescriptor { index, source } => write!(
                f,
                "Blossom list element {index} is not a valid blob descriptor: {source}"
            ),
        }
    }
}

impl std::error::Error for ListError {}

/// The async BUD-02/04/12 blob client. Construction wires the full engine
/// HTTP discipline (module doc); one client may serve many operations.
pub struct BlossomClient {
    http: reqwest::Client,
    max_response_bytes: usize,
    max_list_response_bytes: usize,
}

impl BlossomClient {
    /// Build the HTTP client with platform DNS, no redirects, no retries, no
    /// proxy, no referer, and one overall deadline.
    pub fn new(config: BlossomClientConfig) -> Result<Self, ClientBuildError> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .referer(false)
            .timeout(config.request_deadline)
            .build()
            .map_err(|error| ClientBuildError {
                reason: error.to_string(),
            })?;
        Ok(Self {
            http,
            max_response_bytes: config.max_response_bytes,
            max_list_response_bytes: config.max_list_response_bytes,
        })
    }

    /// `PUT /upload` of `blob`'s exact bytes, self-verifying end to end:
    ///
    /// 1. refuse an authorization that is not an `upload` grant for
    ///    exactly these bytes (no cross-blob/cross-verb replay);
    /// 2. send `Authorization`, `X-SHA-256`, and (when supplied)
    ///    `Content-Type`; `Content-Length` rides the byte body;
    /// 4. stream the response under the configured byte cap;
    /// 5. map every non-success status into the separated taxonomy;
    /// 6. integrity-gate the descriptor's sha256 against the local hash.
    pub async fn upload(
        &self,
        server: &BlossomServerUrl,
        blob: &[u8],
        content_type: Option<&str>,
        auth: &SignedAuthorization,
    ) -> Result<VerifiedUpload, UploadError> {
        let local_hash = Sha256Hash::of(blob);
        if auth.verb() != BlossomVerb::Upload || auth.blob() != Some(local_hash) {
            return Err(UploadError::AuthorizationBlobMismatch {
                expected: local_hash,
                authorized_verb: auth.verb(),
                authorized_blob: auth.blob(),
            });
        }

        let mut request = self
            .http
            .put(server.upload_endpoint())
            .header(reqwest::header::AUTHORIZATION, auth.header_value())
            .header("X-SHA-256", local_hash.to_hex());
        if let Some(content_type) = content_type {
            request = request.header(reqwest::header::CONTENT_TYPE, content_type.to_string());
        }
        let mut response =
            request
                .body(blob.to_vec())
                .send()
                .await
                .map_err(|error| UploadError::Network {
                    detail: error.to_string(),
                })?;

        let status = response.status();
        let reason = x_reason(&response);
        match status.as_u16() {
            200 | 201 => {}
            code if status.is_redirection() => {
                return Err(UploadError::RedirectRefused { status: code });
            }
            401 | 403 => {
                return Err(UploadError::AuthRejected {
                    status: status.as_u16(),
                    reason,
                });
            }
            code if status.is_server_error() => {
                return Err(UploadError::ServerError {
                    status: code,
                    reason,
                });
            }
            code => {
                return Err(UploadError::ServerRejected {
                    status: code,
                    reason,
                });
            }
        }

        let bytes = read_bounded_body(&mut response, self.max_response_bytes)
            .await
            .map_err(|error| match error {
                BodyReadError::Network { detail } => UploadError::Network { detail },
                BodyReadError::TooLarge { limit_bytes } => {
                    UploadError::ResponseTooLarge { limit_bytes }
                }
            })?;

        let descriptor =
            BlobDescriptor::parse_json(&bytes).map_err(UploadError::DescriptorInvalid)?;
        if descriptor.sha256 != local_hash {
            return Err(UploadError::Sha256Mismatch {
                expected: local_hash,
                returned: descriptor.sha256,
            });
        }
        // The bytes are RIGHT HERE, so the exact-byte witness is minted from
        // them -- never from `descriptor.sha256`, which is the server's
        // claim. `url`/`mime_type` ride along as explicitly untrusted
        // presentation text (#884).
        let asset =
            VerifiedAsset::from_bytes(blob, descriptor.url.clone(), descriptor.mime_type.clone());
        debug_assert_eq!(asset.sha256(), descriptor.sha256);
        Ok(VerifiedUpload { descriptor, asset })
    }

    /// `PUT /mirror` (BUD-04, #551): ask `server` to download the blob at
    /// `source_url` itself, self-verifying end to end:
    ///
    /// 1. refuse an authorization that is not an `upload` grant (BUD-04
    ///    mirrors under the `upload` verb) bound to EXACTLY `expected`;
    /// 2. send `Authorization` and the `{"url": ...}` JSON body;
    /// 4. stream the response under the configured byte cap;
    /// 5. map every non-success status into the separated taxonomy -- 409
    ///    (server refused: mirrored hash != authorized `x`) and 502
    ///    (server could not fetch the origin) each keep their own variant;
    /// 6. integrity-gate the 200/201 descriptor's sha256 against `expected`.
    ///
    /// The result is the server's descriptor and NOTHING stronger (#884).
    /// This client never observes the mirrored bytes -- the destination
    /// server fetches them -- so a successful mirror cannot mint an
    /// exact-byte [`nmp_asset::VerifiedAsset`], and therefore cannot return
    /// a [`VerifiedUpload`]. The gate above still proves the returned
    /// descriptor names the hash the caller authorized; it does not prove
    /// anyone hashed real bytes here.
    pub async fn mirror(
        &self,
        server: &BlossomServerUrl,
        source_url: &str,
        expected: Sha256Hash,
        auth: &SignedAuthorization,
    ) -> Result<BlobDescriptor, MirrorError> {
        if auth.verb() != BlossomVerb::Upload || auth.blob() != Some(expected) {
            return Err(MirrorError::AuthorizationBlobMismatch {
                expected,
                authorized_verb: auth.verb(),
                authorized_blob: auth.blob(),
            });
        }

        let body = serde_json::json!({ "url": source_url }).to_string();
        let mut response = self
            .http
            .put(server.mirror_endpoint())
            .header(reqwest::header::AUTHORIZATION, auth.header_value())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|error| MirrorError::Network {
                detail: error.to_string(),
            })?;

        let status = response.status();
        let reason = x_reason(&response);
        match status.as_u16() {
            // 201 newly mirrored, 200 already exists -- both descriptors.
            200 | 201 => {}
            code if status.is_redirection() => {
                return Err(MirrorError::RedirectRefused { status: code });
            }
            401 | 403 => {
                return Err(MirrorError::AuthRejected {
                    status: status.as_u16(),
                    reason,
                });
            }
            409 => {
                return Err(MirrorError::HashMismatchRefused { reason });
            }
            // 502 MUST match before the generic 5xx arm: "could not fetch
            // the origin" is the origin's failure, not the server's.
            502 => {
                return Err(MirrorError::OriginFetchFailed { reason });
            }
            code if status.is_server_error() => {
                return Err(MirrorError::ServerError {
                    status: code,
                    reason,
                });
            }
            code => {
                return Err(MirrorError::ServerRejected {
                    status: code,
                    reason,
                });
            }
        }

        let bytes = read_bounded_body(&mut response, self.max_response_bytes)
            .await
            .map_err(|error| match error {
                BodyReadError::Network { detail } => MirrorError::Network { detail },
                BodyReadError::TooLarge { limit_bytes } => {
                    MirrorError::ResponseTooLarge { limit_bytes }
                }
            })?;

        let descriptor =
            BlobDescriptor::parse_json(&bytes).map_err(MirrorError::DescriptorInvalid)?;
        if descriptor.sha256 != expected {
            return Err(MirrorError::Sha256Mismatch {
                expected,
                returned: descriptor.sha256,
            });
        }
        Ok(descriptor)
    }

    /// `DELETE /<sha256>` (BUD-12, #551):
    ///
    /// 1. refuse an authorization that is not a `delete` grant bound to
    ///    EXACTLY `blob` -- the operation deletes the ONE blob in the
    ///    request path, and BUD-12 mandates that extra `x` tags on the
    ///    token never widen that to other blobs (the
    ///    [`SignedAuthorization`] witness holds exactly one hash);
    /// 2. send the DELETE with the `Authorization` header;
    /// 4. 200/204 succeed; every other status maps into the separated
    ///    taxonomy (404 keeps its own `NotFound` variant).
    ///
    /// SUCCESS-BODY DISCIPLINE: no payload is parsed, so the body is
    /// drained chunk-by-chunk and DISCARDED -- never accumulated, so a
    /// huge body cannot buffer unbounded (memory is bounded by one reqwest
    /// chunk, time by `request_deadline`). Chosen over the capped-reader +
    /// `ResponseTooLarge` alternative because a typed size refusal on
    /// bytes nobody reads would be a dead taxonomy slot. A non-success
    /// response is not drained: dropping it cancels the stream, exactly as
    /// `upload` does.
    pub async fn delete(
        &self,
        server: &BlossomServerUrl,
        blob: Sha256Hash,
        auth: &SignedAuthorization,
    ) -> Result<(), DeleteError> {
        if auth.verb() != BlossomVerb::Delete || auth.blob() != Some(blob) {
            return Err(DeleteError::AuthorizationBlobMismatch {
                expected: blob,
                authorized_verb: auth.verb(),
                authorized_blob: auth.blob(),
            });
        }

        let mut response = self
            .http
            .delete(server.blob_endpoint(blob))
            .header(reqwest::header::AUTHORIZATION, auth.header_value())
            .send()
            .await
            .map_err(|error| DeleteError::Network {
                detail: error.to_string(),
            })?;

        let status = response.status();
        let reason = x_reason(&response);
        match status.as_u16() {
            200 | 204 => {}
            code if status.is_redirection() => {
                return Err(DeleteError::RedirectRefused { status: code });
            }
            401..=403 => {
                return Err(DeleteError::AuthRejected {
                    status: status.as_u16(),
                    reason,
                });
            }
            404 => {
                return Err(DeleteError::NotFound { reason });
            }
            code if status.is_server_error() => {
                return Err(DeleteError::ServerError {
                    status: code,
                    reason,
                });
            }
            code => {
                return Err(DeleteError::ServerRejected {
                    status: code,
                    reason,
                });
            }
        }

        // Drain-and-discard (see method doc): stream the unused success
        // body without ever accumulating it.
        while response
            .chunk()
            .await
            .map_err(|error| DeleteError::Network {
                detail: error.to_string(),
            })?
            .is_some()
        {}
        Ok(())
    }

    /// `GET /list/<pubkey>` (BUD-12, #551): the blobs `server` stores for
    /// `owner`, newest first (the server sorts by `uploaded` descending).
    ///
    /// 1. when `auth` is `Some`, refuse it unless it is a `list` grant
    ///    (typed [`ListError::WrongVerb`], before any admission or I/O);
    ///    when `None`, NO `Authorization` header is sent -- a server that
    ///    requires one answers 401, surfaced as [`ListError::AuthRejected`];
    /// 2. GET with `cursor`/`limit` query parameters only when set;
    /// 4. stream the response under `max_list_response_bytes`;
    /// 5. parse STRICTLY: the top level must be a JSON array, and every
    ///    element must satisfy exactly the [`BlobDescriptor::parse_json`]
    ///    field rules -- one malformed row fails the whole call typed
    ///    ([`ListError::InvalidDescriptor`]), never a silently shortened
    ///    success.
    pub async fn list(
        &self,
        server: &BlossomServerUrl,
        owner: nostr::PublicKey,
        page: &ListPage,
        auth: Option<&SignedAuthorization>,
    ) -> Result<Vec<BlobDescriptor>, ListError> {
        if let Some(auth) = auth {
            if auth.verb() != BlossomVerb::List {
                return Err(ListError::WrongVerb {
                    authorized_verb: auth.verb(),
                });
            }
        }

        let mut request = self.http.get(server.list_endpoint(&owner, page));
        if let Some(auth) = auth {
            request = request.header(reqwest::header::AUTHORIZATION, auth.header_value());
        }
        let mut response = request.send().await.map_err(|error| ListError::Network {
            detail: error.to_string(),
        })?;

        let status = response.status();
        let reason = x_reason(&response);
        match status.as_u16() {
            200 => {}
            code if status.is_redirection() => {
                return Err(ListError::RedirectRefused { status: code });
            }
            401..=403 => {
                return Err(ListError::AuthRejected {
                    status: status.as_u16(),
                    reason,
                });
            }
            code if status.is_server_error() => {
                return Err(ListError::ServerError {
                    status: code,
                    reason,
                });
            }
            code => {
                return Err(ListError::ServerRejected {
                    status: code,
                    reason,
                });
            }
        }

        let bytes = read_bounded_body(&mut response, self.max_list_response_bytes)
            .await
            .map_err(|error| match error {
                BodyReadError::Network { detail } => ListError::Network { detail },
                BodyReadError::TooLarge { limit_bytes } => {
                    ListError::ResponseTooLarge { limit_bytes }
                }
            })?;

        let rows: Vec<serde_json::Value> =
            serde_json::from_slice(&bytes).map_err(|error| ListError::BodyNotAnArray {
                reason: error.to_string(),
            })?;
        let mut descriptors = Vec::with_capacity(rows.len());
        for (index, row) in rows.into_iter().enumerate() {
            let descriptor = BlobDescriptor::from_value(row)
                .map_err(|source| ListError::InvalidDescriptor { index, source })?;
            descriptors.push(descriptor);
        }
        Ok(descriptors)
    }
}

/// The `X-Reason` header servers attach to refusals (BUD-01), captured
/// verbatim into every taxonomy variant that carries a `reason`.
fn x_reason(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get("x-reason")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// [`read_bounded_body`]'s failure modes: the stream died, or the body
/// crossed the cap. Internal; each operation maps these into its own
/// taxonomy so operation failures stay separated.
enum BodyReadError {
    Network { detail: String },
    TooLarge { limit_bytes: usize },
}

/// Stream a response body under `limit` bytes, failing the moment the cap
/// would be crossed -- the ONE bounded reader every payload-bearing
/// operation shares, so no response is ever buffered unbounded.
async fn read_bounded_body(
    response: &mut reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, BodyReadError> {
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| BodyReadError::Network {
            detail: error.to_string(),
        })?
    {
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(BodyReadError::TooLarge { limit_bytes: limit });
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Invariant (#545): a root-path http/https server URL is admitted
    /// (with or without an explicit trailing slash, with a port).
    #[test]
    fn root_path_http_and_https_urls_are_admitted() {
        assert!(BlossomServerUrl::parse("https://cdn.example.com").is_ok());
        assert!(BlossomServerUrl::parse("https://cdn.example.com/").is_ok());
        assert!(BlossomServerUrl::parse("http://127.0.0.1:3000").is_ok());
    }

    /// Invariant (#545): every [`ServerUrlError`] variant is a real,
    /// constructible refusal -- scheme, path, credentials, query/fragment,
    /// hostless, and unparseable inputs each hit their own variant.
    #[test]
    fn each_server_url_refusal_is_its_own_typed_variant() {
        assert!(matches!(
            BlossomServerUrl::parse("not a url"),
            Err(ServerUrlError::Parse { .. })
        ));
        assert_eq!(
            BlossomServerUrl::parse("mailto:user@example.com"),
            Err(ServerUrlError::MissingHost)
        );
        assert_eq!(
            BlossomServerUrl::parse("ftp://cdn.example.com"),
            Err(ServerUrlError::UnsupportedScheme {
                scheme: "ftp".to_string()
            })
        );
        assert_eq!(
            BlossomServerUrl::parse("https://user:secret@cdn.example.com"),
            Err(ServerUrlError::Credentialed)
        );
        assert_eq!(
            BlossomServerUrl::parse("https://cdn.example.com/media"),
            Err(ServerUrlError::NonRootPath {
                path: "/media".to_string()
            })
        );
        assert_eq!(
            BlossomServerUrl::parse("https://cdn.example.com/?a=1"),
            Err(ServerUrlError::QueryOrFragment)
        );
        assert_eq!(
            BlossomServerUrl::parse("https://cdn.example.com/#frag"),
            Err(ServerUrlError::QueryOrFragment)
        );
    }
}
