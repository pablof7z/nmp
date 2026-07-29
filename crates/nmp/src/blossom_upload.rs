//! One-shot engine-authorized Blossom upload (#971).
//!
//! # What this is
//!
//! Swift can already reach the Blossom transport, but only by driving the
//! whole security-sensitive choreography itself: hash the bytes, pick BUD-11
//! timestamps, build kind:24242, invoke the signer, validate the signature,
//! and keep the hash and the bytes paired. That last step is the one a bug can
//! silently break -- hash bytes A, authorize that hash, upload bytes B. This
//! module is the single call that makes it unrepresentable, by composing the
//! three things that already own the problem:
//!
//! ```text
//! nmp_media::prepare  -> the exact bytes, their hash, and the draft, bound
//! Handle::sign_event  -> the governed signer, the one signing door
//! PreparedUpload::upload / nmp_blossom::BlossomClient -> the hardened PUT
//! ```
//!
//! It re-implements none of them.
//!
//! # What this is NOT
//!
//! It is process-local and non-durable, exactly as #971 and #562 settled: no
//! write intent, no receipt, no store row, no retry owner, no scheduler, no
//! third workload noun. Nothing here is queued, admitted, throttled or
//! counted. An upload is one `Handle::sign_event_with_completion` followed by
//! one task on the engine's existing adapter runtime, and the only state that
//! outlives the call is the one-shot result channel.
//!
//! It also sets no policy. There is no size cap, no concurrency cap and no
//! aggregate memory ceiling: how large a blob a user may upload and how many
//! uploads an app runs at once are the app's decisions, not NMP's. The only
//! number this module owns is [`AUTHORIZATION_LIFETIME`], and its doc says
//! exactly which two bounds fix it and why the caller cannot supply it.
//!
//! # Cancellation
//!
//! [`BlossomUploadCancel`] is idempotent and wakes the operation exactly once.
//! Before transmission it withdraws the signer request and no HTTP happens at
//! all. After transmission it is an OBSERVATION GAP, and the taxonomy says so:
//! [`BlossomUploadError::Cancelled`] means the local operation stopped, never
//! that the remote did not store the bytes. Engine shutdown reaches the same
//! two paths through mechanisms that already exist -- the reducer's own
//! sign-event drain, and `EngineThread::join`'s drop of the adapter runtime,
//! which fires this module's transport guard like every other adapter task's.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nmp_blossom::{
    AuthValidationError, BlossomClient, BlossomClientConfig, BlossomServerUrl, BlossomVerb,
    DescriptorError, ExpectedAuthorization, ServerUrlError, SignedAuthorization, UploadError,
};
use nmp_media::{prepare, MediaUploadError, PrepareError, PreparedUpload, UploadedAsset};
use nostr::{PublicKey, Timestamp};

use crate::runtime::{
    AsyncFifoReceiver, EngineClock, FifoReceiver, FifoSender, Handle, SignEventCancel,
    SignEventError,
};
use crate::{fifo_channel, Engine, EngineError};

/// How long a BUD-11 upload authorization NMP mints stays valid.
///
/// This is the one number this module chooses, so it owes an explanation.
///
/// BUD-11 REQUIRES an `expiration` tag, and `nmp_media::prepare` refuses a
/// window that is already closed, so a lifetime has to exist for the mechanism
/// to work at all. #971's required public shape forbids the caller supplying
/// one -- the semantic surface accepts no timestamp and no expiration, because
/// accepting them is how an app ends up choosing a window that its own signer
/// then outlives.
///
/// Two bounds fix the value:
///
/// - It MUST exceed `nmp_blossom::DEFAULT_REQUEST_DEADLINE`, the longest the
///   authorized request itself may run. An authorization that expired before
///   its own request could finish would be unusable by construction; that
///   floor is pinned by
///   `authorization_lifetime_outlives_the_request_deadline_it_authorizes`.
/// - Above that floor it has to cover SIGNER latency, which NMP cannot bound
///   at all: the governed signing capability may live in another process, on
///   another device, or behind a human deciding whether to approve. Five
///   minutes is a human-approval window.
///
/// No NMP behaviour changes as it moves between those bounds, and nothing the
/// caller does is capped by it.
const AUTHORIZATION_LIFETIME: Duration = Duration::from_secs(5 * 60);

/// The semantic inputs to one upload: what the user picked, how it should be
/// described, and where it goes.
///
/// Author, current time, blob hash, event kind, tags, authorization window,
/// signature and HTTP headers are deliberately absent -- every one of them is
/// NMP's to decide, and a caller that could state one could state a wrong one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlossomUploadRequest {
    /// The Blossom server's base URL, as the operator/product configured it.
    pub server_url: String,
    /// The exact bytes to upload. Hashed once, in Rust, and uploaded verbatim.
    pub bytes: Vec<u8>,
    /// The governed MIME type these bytes are uploaded with.
    pub content_type: String,
    /// The product's human-readable description of the action, carried as the
    /// BUD-11 authorization's `content`.
    pub description: String,
}

/// Every way one engine-authorized upload can fail.
///
/// Exhaustive and un-flattened: the signer taxonomy, the BUD-11 time taxonomy
/// and the Blossom transport/integrity taxonomy each keep their own variants
/// rather than collapsing into strings. Every variant is constructible by a
/// falsifier in this module's test suite.
///
/// The dead low-level cases the composition already rules out (wrong kind,
/// missing tags, an authorization bound to a different body) are NOT variants:
/// `nmp_media::prepare` builds the draft and `SignedAuthorization::validate`
/// re-checks it, so a signer that returned something else is one fact --
/// [`Self::InvalidSignerOutput`] -- not a second protocol taxonomy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlossomUploadError {
    /// `server_url` is not an admissible Blossom base URL.
    InvalidServerUrl(ServerUrlError),
    /// `content_type` is empty; NIP-68 imeta and BUD-02 both need one.
    EmptyContentType,
    /// NMP could not compose a usable BUD-11 window at this instant --
    /// `created_at + lifetime` is not representable. Reachable only from an
    /// engine clock reading at the top of the `u64` range.
    AuthorizationWindow {
        created_at_secs: u64,
        lifetime_secs: u64,
    },
    /// No active account, so there is no author to authorize as and no signer
    /// to ask. Refused before any hashing or HTTP.
    NoActiveSigner,
    /// The active signer exists but could not be reached.
    SignerUnavailable { reason: String },
    /// The active signer was reached and refused.
    SignerRejected { reason: String },
    /// The signer returned something that is not a valid signature over the
    /// exact authorization NMP composed.
    InvalidSignerOutput { reason: String },
    /// The authorization's window closed before the upload could use it --
    /// typically a signer that took longer than [`AUTHORIZATION_LIFETIME`].
    AuthorizationExpired { expiration_secs: u64, now_secs: u64 },
    /// The engine clock moved backwards between composing the authorization
    /// and validating it, so a freshly minted `created_at` is in the future.
    /// Distinct from every signer fault: the signer did nothing wrong.
    ClockMovedBackward { created_at_secs: u64, now_secs: u64 },
    /// The Blossom HTTP stack could not be constructed.
    ClientBuild { reason: String },
    /// A loopback/private/link-local/onion destination without the operator's
    /// opt-in. Refused before any socket I/O.
    LocalHostNotAdmitted { host: String },
    /// Connect/DNS/TLS/timeout, or the body stream died.
    Network { detail: String },
    /// The server answered with a redirect; redirects are never followed.
    RedirectRefused { status: u16 },
    /// 401/403: the server refused the authorization itself.
    AuthRejected { status: u16, reason: Option<String> },
    /// Any other non-success, non-5xx status.
    ServerRejected { status: u16, reason: Option<String> },
    /// 5xx.
    ServerError { status: u16, reason: Option<String> },
    /// The descriptor response exceeded the streamed response bound.
    ResponseTooLarge { limit_bytes: usize },
    /// The BUD-02 descriptor did not parse or did not satisfy its own rules.
    DescriptorInvalid(DescriptorError),
    /// The server's descriptor claims a hash that is not the hash of the bytes
    /// NMP uploaded. The integrity gate, crossing the semantic operation.
    Sha256Mismatch {
        expected_sha256_hex: String,
        returned_sha256_hex: String,
    },
    /// The engine was closed before or during the operation.
    EngineClosed,
    /// The operation was withdrawn. If the request had already been
    /// transmitted this is an OBSERVATION GAP: the local operation stopped,
    /// and whether the remote stored the bytes is unknown and unclaimed.
    Cancelled,
}

impl std::fmt::Display for BlossomUploadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidServerUrl(error) => {
                write!(formatter, "invalid Blossom server URL: {error}")
            }
            Self::EmptyContentType => formatter.write_str("Blossom upload content type is empty"),
            Self::AuthorizationWindow {
                created_at_secs,
                lifetime_secs,
            } => write!(
                formatter,
                "no representable BUD-11 authorization window at {created_at_secs} for a \
                 {lifetime_secs}s lifetime"
            ),
            Self::NoActiveSigner => formatter.write_str("no active signer"),
            Self::SignerUnavailable { reason } => write!(formatter, "signer unavailable: {reason}"),
            Self::SignerRejected { reason } => {
                write!(formatter, "signer rejected request: {reason}")
            }
            Self::InvalidSignerOutput { reason } => {
                write!(formatter, "signer returned invalid output: {reason}")
            }
            Self::AuthorizationExpired {
                expiration_secs,
                now_secs,
            } => write!(
                formatter,
                "Blossom authorization expired at {expiration_secs}; current time is {now_secs}"
            ),
            Self::ClockMovedBackward {
                created_at_secs,
                now_secs,
            } => write!(
                formatter,
                "clock moved backward after composition: created_at {created_at_secs}, current \
                 time {now_secs}"
            ),
            Self::ClientBuild { reason } => {
                write!(formatter, "Blossom HTTP client construction failed: {reason}")
            }
            Self::LocalHostNotAdmitted { host } => {
                write!(formatter, "Blossom destination host {host:?} is not admitted")
            }
            Self::Network { detail } => write!(formatter, "Blossom transport failed: {detail}"),
            Self::RedirectRefused { status } => {
                write!(formatter, "Blossom redirect is refused (HTTP {status})")
            }
            Self::AuthRejected { status, reason } => write!(
                formatter,
                "Blossom server rejected authorization (HTTP {status}, reason {reason:?})"
            ),
            Self::ServerRejected { status, reason } => write!(
                formatter,
                "Blossom server rejected the upload (HTTP {status}, reason {reason:?})"
            ),
            Self::ServerError { status, reason } => write!(
                formatter,
                "Blossom server failed (HTTP {status}, reason {reason:?})"
            ),
            Self::ResponseTooLarge { limit_bytes } => write!(
                formatter,
                "Blossom descriptor response exceeds {limit_bytes} bytes"
            ),
            Self::DescriptorInvalid(error) => {
                write!(formatter, "Blossom descriptor is invalid: {error}")
            }
            Self::Sha256Mismatch {
                expected_sha256_hex,
                returned_sha256_hex,
            } => write!(
                formatter,
                "Blossom descriptor hash {returned_sha256_hex} does not match uploaded bytes \
                 {expected_sha256_hex}"
            ),
            Self::EngineClosed => formatter.write_str("engine is closed"),
            Self::Cancelled => formatter.write_str(
                "Blossom upload was cancelled; if bytes were transmitted, remote storage is unknown",
            ),
        }
    }
}

impl std::error::Error for BlossomUploadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidServerUrl(error) => Some(error),
            Self::DescriptorInvalid(error) => Some(error),
            _ => None,
        }
    }
}

fn sign_error(error: SignEventError) -> BlossomUploadError {
    match error {
        SignEventError::NoActiveSigner => BlossomUploadError::NoActiveSigner,
        SignEventError::InvalidRequest { reason }
        | SignEventError::InvalidSignerOutput { reason } => {
            BlossomUploadError::InvalidSignerOutput { reason }
        }
        SignEventError::SignerUnavailable { reason } => {
            BlossomUploadError::SignerUnavailable { reason }
        }
        SignEventError::SignerRejected { reason } => BlossomUploadError::SignerRejected { reason },
        SignEventError::EngineClosed => BlossomUploadError::EngineClosed,
        SignEventError::Cancelled => BlossomUploadError::Cancelled,
    }
}

/// The two time facts stay their own variants; everything else a BUD-11
/// validation can complain about is, at THIS seam, one fact: the governed
/// signer did not sign the exact draft NMP composed.
fn authorization_error(error: AuthValidationError) -> BlossomUploadError {
    match error {
        AuthValidationError::Expired { expiration, now } => {
            BlossomUploadError::AuthorizationExpired {
                expiration_secs: expiration.as_secs(),
                now_secs: now.as_secs(),
            }
        }
        AuthValidationError::CreatedAtInFuture { created_at, now } => {
            BlossomUploadError::ClockMovedBackward {
                created_at_secs: created_at.as_secs(),
                now_secs: now.as_secs(),
            }
        }
        other => BlossomUploadError::InvalidSignerOutput {
            reason: format!("signer output is not the authorization NMP composed: {other}"),
        },
    }
}

fn upload_error(error: UploadError) -> BlossomUploadError {
    match error {
        UploadError::AuthorizationBlobMismatch {
            expected,
            authorized_verb,
            authorized_blob,
        } => BlossomUploadError::InvalidSignerOutput {
            reason: format!(
                "authorization/body binding changed before transport: expected {}, verb \
                 {authorized_verb}, blob {:?}",
                expected.to_hex(),
                authorized_blob.map(|hash| hash.to_hex())
            ),
        },
        UploadError::LocalHostNotAdmitted { host } => {
            BlossomUploadError::LocalHostNotAdmitted { host }
        }
        UploadError::Network { detail } => BlossomUploadError::Network { detail },
        UploadError::RedirectRefused { status } => BlossomUploadError::RedirectRefused { status },
        UploadError::AuthRejected { status, reason } => {
            BlossomUploadError::AuthRejected { status, reason }
        }
        UploadError::ServerRejected { status, reason } => {
            BlossomUploadError::ServerRejected { status, reason }
        }
        UploadError::ServerError { status, reason } => {
            BlossomUploadError::ServerError { status, reason }
        }
        UploadError::ResponseTooLarge { limit_bytes } => {
            BlossomUploadError::ResponseTooLarge { limit_bytes }
        }
        UploadError::DescriptorInvalid(error) => BlossomUploadError::DescriptorInvalid(error),
        UploadError::Sha256Mismatch { expected, returned } => BlossomUploadError::Sha256Mismatch {
            expected_sha256_hex: expected.to_hex(),
            returned_sha256_hex: returned.to_hex(),
        },
    }
}

type UploadResult = Result<UploadedAsset, BlossomUploadError>;

/// Everything one upload holds, behind one mutex. Two withdrawal slots rather
/// than one because the signer completion can fire BEFORE
/// `sign_event_with_completion` has even returned the token for it: the
/// transport handle is then armed first, and a single slot would let the late
/// signer token overwrite it and leave the request unstoppable.
struct TerminalState {
    /// The one-shot result channel. `None` once the result has been delivered.
    sender: Option<FifoSender<UploadResult>>,
    signer: Option<SignEventCancel>,
    transport: Option<tokio::task::AbortHandle>,
    finished: bool,
}

struct UploadTerminal {
    state: Mutex<TerminalState>,
}

impl UploadTerminal {
    fn new(sender: FifoSender<UploadResult>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(TerminalState {
                sender: Some(sender),
                signer: None,
                transport: None,
                finished: false,
            }),
        })
    }

    fn is_open(&self) -> bool {
        !self.state.lock().unwrap().finished
    }

    /// Deliver the single result if this call is the one that closes the
    /// operation. Returns whether it won.
    fn settle(&self, result: UploadResult) -> bool {
        let sender = {
            let mut state = self.state.lock().unwrap();
            if state.finished {
                return false;
            }
            state.finished = true;
            state.sender.take()
        };
        if let Some(sender) = sender {
            sender.send(result);
        }
        true
    }

    /// Withdraw the operation: deliver `Cancelled` if nothing else has been
    /// delivered yet, and stop whatever is currently running either way.
    /// Idempotent, and safe after completion -- cancelling a finished signer
    /// operation and aborting a finished task are both no-ops.
    fn cancel(&self) {
        let (sender, signer, transport) = {
            let mut state = self.state.lock().unwrap();
            let sender = (!state.finished).then(|| state.sender.take()).flatten();
            state.finished = true;
            (sender, state.signer.take(), state.transport.take())
        };
        if let Some(sender) = sender {
            sender.send(Err(BlossomUploadError::Cancelled));
        }
        if let Some(signer) = signer {
            signer.cancel();
        }
        if let Some(transport) = transport {
            transport.abort();
        }
    }

    fn arm_signer(&self, cancel: SignEventCancel) {
        let mut state = self.state.lock().unwrap();
        if state.finished {
            drop(state);
            cancel.cancel();
            return;
        }
        state.signer = Some(cancel);
    }

    /// Arm the transport withdrawal and release the signer's, which by this
    /// point has already completed.
    fn arm_transport(&self, abort: tokio::task::AbortHandle) {
        let mut state = self.state.lock().unwrap();
        state.signer = None;
        if state.finished {
            drop(state);
            abort.abort();
            return;
        }
        state.transport = Some(abort);
    }
}

/// Idempotent withdrawal token for one exact upload.
///
/// Withdrawing after the request has been transmitted is an observation gap,
/// not a rollback -- see [`BlossomUploadError::Cancelled`].
#[derive(Clone)]
pub struct BlossomUploadCancel {
    terminal: Arc<UploadTerminal>,
}

impl BlossomUploadCancel {
    pub fn cancel(&self) {
        self.terminal.cancel();
    }
}

impl std::fmt::Debug for BlossomUploadCancel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BlossomUploadCancel")
            .field("open", &self.terminal.is_open())
            .finish()
    }
}

/// One live process-local Blossom upload. It creates no durable write,
/// receipt, retry owner, outbox lane or relay publication, and dropping it
/// without consuming the result withdraws it.
pub struct BlossomUploadOperation {
    result: Option<FifoReceiver<UploadResult>>,
    cancel: Option<BlossomUploadCancel>,
}

impl std::fmt::Debug for BlossomUploadOperation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BlossomUploadOperation")
            .field("pending", &self.result.is_some())
            .finish()
    }
}

impl BlossomUploadOperation {
    /// Block until the single outcome arrives. A closed channel means the
    /// operation was withdrawn without a result of its own.
    pub fn recv(mut self) -> UploadResult {
        self.result
            .take()
            .expect("a Blossom upload result is consumed exactly once")
            .recv()
            .unwrap_or(Err(BlossomUploadError::Cancelled))
    }

    /// A token that withdraws THIS upload, usable from another thread while
    /// [`Self::recv`] blocks.
    #[must_use]
    pub fn cancel_handle(&self) -> BlossomUploadCancel {
        self.cancel
            .as_ref()
            .expect("a live Blossom upload retains its cancel handle")
            .clone()
    }

    /// The async halves, for the FFI boundary's one-shot handle. Doc-hidden:
    /// direct Rust uses [`Self::recv`] and [`Self::cancel_handle`].
    #[doc(hidden)]
    pub fn into_async(mut self) -> (BlossomUploadCancel, AsyncFifoReceiver<UploadResult>) {
        let cancel = self
            .cancel
            .take()
            .expect("a live Blossom upload retains its cancel handle");
        let result = self
            .result
            .take()
            .expect("a Blossom upload result is consumed exactly once")
            .into_async();
        (cancel, result)
    }
}

impl Drop for BlossomUploadOperation {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
    }
}

/// Fires if the transport task's future is DROPPED rather than run to
/// completion -- which is exactly what `EngineThread::join` does to every
/// adapter task when the engine shuts down. Without it, shutdown mid-request
/// would leave the operation waiting on a result nobody will ever send.
struct TransportGuard {
    terminal: Arc<UploadTerminal>,
    armed: bool,
}

impl Drop for TransportGuard {
    fn drop(&mut self) {
        if self.armed {
            self.terminal.cancel();
        }
    }
}

/// Everything one upload needs, frozen at one instant under the engine's
/// lifecycle mutex. Constructed only by
/// [`Engine::with_blossom_upload_context`].
pub(crate) struct BlossomUploadContext<'a> {
    pub(crate) author: Option<PublicKey>,
    pub(crate) handle: &'a Handle,
    pub(crate) runtime: tokio::runtime::Handle,
    pub(crate) clock: EngineClock,
    pub(crate) allowed_local_hosts: BTreeSet<String>,
}

impl Engine {
    /// Upload one blob to a Blossom server, authorized by the active governed
    /// signer (#971).
    ///
    /// NMP owns the whole transaction: author and time come from the engine,
    /// the bytes are hashed once and uploaded verbatim, the BUD-11
    /// authorization is composed, signed and re-validated before any HTTP, and
    /// the existing hardened transport performs the request. The caller
    /// supplies only product inputs.
    ///
    /// Returns synchronously with either a typed refusal that made ZERO HTTP
    /// requests, or a live [`BlossomUploadOperation`] to consume once.
    pub fn upload_blossom(
        &self,
        request: BlossomUploadRequest,
    ) -> Result<BlossomUploadOperation, BlossomUploadError> {
        let server = BlossomServerUrl::parse(&request.server_url)
            .map_err(BlossomUploadError::InvalidServerUrl)?;
        self.with_blossom_upload_context(move |context| {
            let author = context.author.ok_or(BlossomUploadError::NoActiveSigner)?;
            let created_at = context.clock.now();
            let lifetime_secs = AUTHORIZATION_LIFETIME.as_secs();
            let window = || BlossomUploadError::AuthorizationWindow {
                created_at_secs: created_at.as_secs(),
                lifetime_secs,
            };
            let expiration = created_at
                .as_secs()
                .checked_add(lifetime_secs)
                .map(Timestamp::from)
                .ok_or_else(window)?;
            let prepared = prepare(
                request.bytes,
                request.content_type,
                author,
                created_at,
                expiration,
                &request.description,
            )
            .map_err(|error| match error {
                PrepareError::EmptyMimeType => BlossomUploadError::EmptyContentType,
                // The composer's only other refusal is a window that closed at
                // birth, which is the same fact `checked_add` reports above.
                PrepareError::Authorization(_) => window(),
            })?;
            start_upload(&context, prepared, server)
        })
        // `with_blossom_upload_context` fails for exactly one reason.
        .map_err(|error| match error {
            EngineError::EngineClosed => BlossomUploadError::EngineClosed,
            other => unreachable!("the upload context has one failure mode, not {other}"),
        })?
    }
}

fn start_upload(
    context: &BlossomUploadContext<'_>,
    prepared: PreparedUpload,
    server: BlossomServerUrl,
) -> Result<BlossomUploadOperation, BlossomUploadError> {
    let draft = prepared.authorization_draft().clone();
    let expected = ExpectedAuthorization {
        verb: BlossomVerb::Upload,
        blob: Some(prepared.sha256()),
    };
    // The operator's local-host opt-in is the ONLY knob taken from the engine:
    // a Blossom server is admitted on exactly the terms a relay is. Every
    // other transport bound stays `nmp-blossom`'s own.
    let config = BlossomClientConfig {
        allowed_local_hosts: context.allowed_local_hosts.clone(),
        ..BlossomClientConfig::default()
    };
    let clock = context.clock.clone();
    let runtime = context.runtime.clone();

    let (sender, receiver) = fifo_channel();
    let terminal = UploadTerminal::new(sender);
    let cancel = BlossomUploadCancel {
        terminal: Arc::clone(&terminal),
    };
    let signing = Arc::clone(&terminal);
    let signer_cancel = context
        .handle
        .sign_event_with_completion(draft, move |signed| {
            let signed = match signed {
                Ok(signed) => signed,
                Err(error) => {
                    signing.settle(Err(sign_error(error)));
                    return;
                }
            };
            // Re-validate what came back against the draft NMP composed --
            // author, signature, verb, exact hash and time -- before any HTTP.
            let authorization = match SignedAuthorization::validate(signed, &expected, clock.now())
            {
                Ok(authorization) => authorization,
                Err(error) => {
                    signing.settle(Err(authorization_error(error)));
                    return;
                }
            };
            // A withdrawal that landed during signing stops here: no request
            // is ever transmitted, so there is no observation gap to report.
            if !signing.is_open() {
                return;
            }
            let transport = Arc::clone(&signing);
            let task = runtime.spawn(async move {
                let mut guard = TransportGuard {
                    terminal: Arc::clone(&transport),
                    armed: true,
                };
                let result = match BlossomClient::new(config) {
                    Ok(client) => prepared
                        .upload(&client, &server, &authorization)
                        .await
                        .map_err(|MediaUploadError::Blossom(error)| upload_error(error)),
                    Err(error) => Err(BlossomUploadError::ClientBuild {
                        reason: error.reason,
                    }),
                };
                guard.armed = false;
                transport.settle(result);
            });
            signing.arm_transport(task.abort_handle());
        })
        .map_err(sign_error)?;
    terminal.arm_signer(signer_cancel);
    Ok(BlossomUploadOperation {
        result: Some(receiver),
        cancel: Some(cancel),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    use nmp_asset::Sha256Hash;
    use nostr::{Event, JsonUtil, Keys, Tag, UnsignedEvent};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::sync::Condvar;
    use std::time::Duration;

    /// The mechanically required floor under [`AUTHORIZATION_LIFETIME`]: an
    /// authorization that expired before its own request deadline could elapse
    /// would be unusable by construction.
    #[test]
    fn authorization_lifetime_outlives_the_request_deadline_it_authorizes() {
        assert!(
            AUTHORIZATION_LIFETIME > nmp_blossom::DEFAULT_REQUEST_DEADLINE,
            "a BUD-11 window shorter than the request it authorizes cannot be used"
        );
        assert_eq!(
            BlossomClientConfig::default().request_deadline,
            nmp_blossom::DEFAULT_REQUEST_DEADLINE,
            "the deadline the floor is measured against is the one this seam configures"
        );
    }

    struct CapturedRequest {
        head: String,
        body: Vec<u8>,
    }

    struct TestServer {
        url: String,
        captured: mpsc::Receiver<CapturedRequest>,
        release: Option<mpsc::Sender<()>>,
        join: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.release.take();
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }

    fn read_request(stream: &mut TcpStream) -> CapturedRequest {
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        let (header_end, content_length) = loop {
            let read = stream.read(&mut chunk).expect("request read");
            assert!(read > 0, "client closed before complete request");
            bytes.extend_from_slice(&chunk[..read]);
            if let Some(marker) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                let header_end = marker + 4;
                let head = String::from_utf8_lossy(&bytes[..header_end]);
                let content_length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                break (header_end, content_length);
            }
        };
        while bytes.len() < header_end + content_length {
            let read = stream.read(&mut chunk).expect("request body read");
            assert!(read > 0, "client closed before complete body");
            bytes.extend_from_slice(&chunk[..read]);
        }
        CapturedRequest {
            head: String::from_utf8(bytes[..header_end].to_vec()).unwrap(),
            body: bytes[header_end..header_end + content_length].to_vec(),
        }
    }

    fn spawn_server(status: &str, response_body: Vec<u8>, gated: bool) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (captured_tx, captured_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let status = status.to_string();
        let join = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("upload connection");
            let request = read_request(&mut stream);
            captured_tx.send(request).unwrap();
            if gated {
                let _ = release_rx.recv_timeout(Duration::from_secs(3));
            }
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                response_body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&response_body);
        });
        TestServer {
            url: format!("http://{address}"),
            captured: captured_rx,
            release: gated.then_some(release_tx),
            join: Some(join),
        }
    }

    fn response_descriptor(hash: Sha256Hash, size: usize, mime: &str) -> Vec<u8> {
        let hash = hash.to_hex();
        format!(
            r#"{{"url":"https://cdn.example/{hash}","sha256":"{hash}","size":{size},"type":"{mime}"}}"#
        )
        .into_bytes()
    }

    fn header<'a>(request: &'a CapturedRequest, wanted: &str) -> &'a str {
        request
            .head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case(wanted).then(|| value.trim())
            })
            .unwrap_or_else(|| panic!("missing {wanted} header"))
    }

    fn test_engine(allow_local: bool) -> Engine {
        Engine::new(crate::EngineConfig {
            allowed_local_relay_hosts: if allow_local {
                vec![" 127.0.0.1. ".to_string()]
            } else {
                Vec::new()
            },
            ..crate::EngineConfig::default()
        })
        .unwrap()
    }

    /// State what time the engine is running at. This is the engine's OWN
    /// clock -- the same value `features/writes/` states an instant with --
    /// not a test double this module invented for itself.
    fn pin_clock(engine: &Engine, secs: u64) -> EngineClock {
        let clock = engine
            .with_blossom_upload_context(|context| context.clock)
            .expect("an open engine has a clock");
        clock.set(Timestamp::from(secs));
        clock
    }

    fn add_active_account(engine: &Engine, scalar: u8) -> PublicKey {
        let registration = engine.add_account(&format!("{scalar:064x}")).unwrap();
        let author = registration.public_key();
        engine.set_active_account(Some(author)).unwrap();
        author
    }

    fn request(server_url: String, bytes: Vec<u8>) -> BlossomUploadRequest {
        BlossomUploadRequest {
            server_url,
            bytes,
            content_type: "application/pdf".to_string(),
            description: "Upload the signed report".to_string(),
        }
    }

    fn signer_public_key(public_key: PublicKey) -> crate::SignerPublicKey {
        crate::SignerPublicKey::new(public_key.to_bytes())
    }

    fn signer_unsigned_to_nostr(unsigned: crate::SignerUnsignedEvent) -> UnsignedEvent {
        let (public_key, created_at, kind, tags, content) = unsigned.into_parts();
        UnsignedEvent::new(
            PublicKey::from_slice(public_key.as_bytes()).unwrap(),
            Timestamp::from(created_at),
            nostr::Kind::from(kind),
            tags.into_iter()
                .map(Tag::parse)
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            content,
        )
    }

    fn nostr_signed_to_signer(event: Event) -> crate::SignerSignedEvent {
        crate::SignerSignedEvent::new(
            event.id.to_bytes(),
            signer_public_key(event.pubkey),
            event.created_at.as_secs(),
            event.kind.as_u16(),
            event.tags.into_iter().map(Tag::to_vec).collect(),
            event.content,
            event.sig.serialize(),
        )
    }

    struct ErrorSigner {
        public_key: PublicKey,
        error: crate::SignerError,
    }

    impl crate::SigningCapability for ErrorSigner {
        fn public_key(&self) -> Option<crate::SignerPublicKey> {
            Some(signer_public_key(self.public_key))
        }

        fn sign(
            &self,
            _unsigned: crate::SignerUnsignedEvent,
        ) -> crate::SignerOp<crate::SignerSignedEvent> {
            crate::SignerOp::err(self.error.clone())
        }
    }

    type PendingResolution = (
        crate::PendingSignerSender<crate::SignerSignedEvent>,
        crate::SignerUnsignedEvent,
    );

    #[derive(Default)]
    struct PendingProbe {
        value: Mutex<Option<PendingResolution>>,
        changed: Condvar,
    }

    impl PendingProbe {
        fn store(&self, value: PendingResolution) {
            *self.value.lock().unwrap() = Some(value);
            self.changed.notify_all();
        }

        fn wait_until_present(&self) {
            let value = self.value.lock().unwrap();
            let (value, timeout) = self
                .changed
                .wait_timeout_while(value, Duration::from_secs(2), |value| value.is_none())
                .unwrap();
            assert!(!timeout.timed_out(), "sign request must become pending");
            assert!(value.is_some());
        }

        fn take(&self) -> PendingResolution {
            let value = self.value.lock().unwrap();
            let (mut value, timeout) = self
                .changed
                .wait_timeout_while(value, Duration::from_secs(2), |value| value.is_none())
                .unwrap();
            assert!(!timeout.timed_out(), "sign request must become pending");
            value.take().unwrap()
        }
    }

    #[derive(Default)]
    struct CancelProbe {
        count: Mutex<usize>,
        changed: Condvar,
    }

    impl CancelProbe {
        fn record(&self) {
            *self.count.lock().unwrap() += 1;
            self.changed.notify_all();
        }

        fn wait_for_one(&self) {
            let count = self.count.lock().unwrap();
            let (count, timeout) = self
                .changed
                .wait_timeout_while(count, Duration::from_secs(2), |count| *count == 0)
                .unwrap();
            assert!(!timeout.timed_out(), "signer cancellation must arrive");
            assert_eq!(*count, 1);
        }
    }

    struct PendingSigner {
        keys: Keys,
        pending: Arc<PendingProbe>,
        cancellations: Arc<CancelProbe>,
    }

    impl crate::SigningCapability for PendingSigner {
        fn public_key(&self) -> Option<crate::SignerPublicKey> {
            Some(signer_public_key(self.keys.public_key()))
        }

        fn sign(
            &self,
            unsigned: crate::SignerUnsignedEvent,
        ) -> crate::SignerOp<crate::SignerSignedEvent> {
            let cancellations = Arc::clone(&self.cancellations);
            let (sender, operation) = crate::SignerOp::pending_channel_with_cancel(move || {
                cancellations.record();
            });
            self.pending.store((sender, unsigned));
            operation
        }
    }

    fn pending_signer(
        engine: &Engine,
    ) -> (
        Keys,
        Arc<PendingProbe>,
        Arc<CancelProbe>,
        crate::SignerRegistration,
    ) {
        let keys = Keys::generate();
        let pending = Arc::new(PendingProbe::default());
        let cancellations = Arc::new(CancelProbe::default());
        let registration = engine
            .add_signer(PendingSigner {
                keys: keys.clone(),
                pending: Arc::clone(&pending),
                cancellations: Arc::clone(&cancellations),
            })
            .unwrap();
        engine.set_active_account(Some(keys.public_key())).unwrap();
        (keys, pending, cancellations, registration)
    }

    fn complete_pending(keys: &Keys, pending: &PendingProbe) {
        let (sender, unsigned) = pending.take();
        let signed = signer_unsigned_to_nostr(unsigned)
            .sign_with_keys(keys)
            .unwrap();
        sender
            .resolve(Ok(nostr_signed_to_signer(signed)))
            .expect("pending signer completion must win");
    }

    /// #971's central falsifier: ONE Rust call owns the exact bytes, the
    /// author, the BUD-11 header and the wire request. Nothing in the request
    /// vocabulary could have named any of them.
    #[test]
    fn exact_bytes_author_and_bud11_header_are_owned_by_one_rust_operation() {
        let bytes = b"%PDF exact bytes\r\nbinary\0payload".to_vec();
        let hash = Sha256Hash::of(&bytes);
        let server = spawn_server(
            "201 Created",
            response_descriptor(hash, bytes.len(), "application/pdf"),
            false,
        );
        let engine = test_engine(true);
        let author = add_active_account(&engine, 17);
        pin_clock(&engine, 1_700_000_000);

        let uploaded = engine
            .upload_blossom(request(server.url.clone(), bytes.clone()))
            .unwrap()
            .recv()
            .unwrap();
        let captured = server
            .captured
            .recv_timeout(Duration::from_secs(2))
            .unwrap();

        assert!(captured.head.starts_with("PUT /upload HTTP/1.1\r\n"));
        assert_eq!(captured.body, bytes);
        assert_eq!(header(&captured, "content-type"), "application/pdf");
        assert_eq!(header(&captured, "x-sha-256"), hash.to_hex());
        let authorization = header(&captured, "authorization")
            .strip_prefix("Nostr ")
            .expect("BUD-11 header prefix");
        let event_json = URL_SAFE_NO_PAD.decode(authorization).unwrap();
        let event = Event::from_json(event_json).unwrap();
        assert_eq!(event.pubkey, author);
        assert_eq!(event.kind, nostr::Kind::BlossomAuth);
        assert_eq!(event.created_at, Timestamp::from(1_700_000_000));
        let tags: Vec<Vec<String>> = event.tags.into_iter().map(Tag::to_vec).collect();
        assert!(tags.contains(&vec!["t".to_string(), "upload".to_string()]));
        assert!(tags.contains(&vec!["x".to_string(), hash.to_hex()]));
        assert!(tags.contains(&vec![
            "expiration".to_string(),
            (1_700_000_000 + AUTHORIZATION_LIFETIME.as_secs()).to_string()
        ]));
        assert_eq!(uploaded.sha256(), hash);
        assert_eq!(uploaded.descriptor().size, bytes.len() as u64);
        engine.shutdown();
    }

    /// A blob far larger than the caps this operation used to invent is an
    /// ordinary upload: NMP does not decide how much a user may upload. The
    /// bytes reach the wire exactly once and exactly as given.
    #[test]
    fn no_size_or_concurrency_ceiling_stands_between_the_caller_and_the_server() {
        let bytes: Vec<u8> = (0..(20 * 1024 * 1024_usize))
            .map(|index| (index % 251) as u8)
            .collect();
        let hash = Sha256Hash::of(&bytes);
        let server = spawn_server(
            "201 Created",
            response_descriptor(hash, bytes.len(), "application/pdf"),
            false,
        );
        let engine = test_engine(true);
        add_active_account(&engine, 22);
        pin_clock(&engine, 1_700_000_000);

        let uploaded = engine
            .upload_blossom(request(server.url.clone(), bytes.clone()))
            .unwrap()
            .recv()
            .unwrap();
        let captured = server
            .captured
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert_eq!(captured.body.len(), bytes.len());
        assert_eq!(Sha256Hash::of(&captured.body), hash);
        assert_eq!(uploaded.sha256(), hash);
        engine.shutdown();
    }

    /// Many uploads may be in flight at once. Nothing admits, queues or
    /// refuses them, so the only limit is what the app starts.
    #[test]
    fn several_uploads_run_concurrently_without_an_admission_owner() {
        let engine = test_engine(true);
        add_active_account(&engine, 23);
        pin_clock(&engine, 1_700_000_000);
        let servers: Vec<_> = (0..6_u8)
            .map(|index| {
                let bytes = vec![index; 64];
                let hash = Sha256Hash::of(&bytes);
                let server = spawn_server(
                    "201 Created",
                    response_descriptor(hash, bytes.len(), "application/pdf"),
                    false,
                );
                (bytes, hash, server)
            })
            .collect();
        let operations: Vec<_> = servers
            .iter()
            .map(|(bytes, _, server)| {
                engine
                    .upload_blossom(request(server.url.clone(), bytes.clone()))
                    .expect("no admission owner may refuse a concurrent upload")
            })
            .collect();
        for (operation, (_, hash, _)) in operations.into_iter().zip(servers.iter()) {
            assert_eq!(operation.recv().unwrap().sha256(), *hash);
        }
        engine.shutdown();
    }

    /// Every pre-flight refusal is typed and reaches the network zero times.
    #[test]
    fn preflight_refusals_are_typed_and_make_zero_http_requests() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let engine = test_engine(true);

        assert_eq!(
            engine
                .upload_blossom(request(
                    "ftp://blobs.example".to_string(),
                    b"scheme".to_vec()
                ))
                .unwrap_err(),
            BlossomUploadError::InvalidServerUrl(nmp_blossom::ServerUrlError::UnsupportedScheme {
                scheme: "ftp".to_string()
            })
        );
        assert_eq!(
            engine
                .upload_blossom(request(url.clone(), b"no signer".to_vec()))
                .unwrap_err(),
            BlossomUploadError::NoActiveSigner
        );

        add_active_account(&engine, 18);
        let mut empty = request(url.clone(), b"empty type".to_vec());
        empty.content_type.clear();
        assert_eq!(
            engine.upload_blossom(empty).unwrap_err(),
            BlossomUploadError::EmptyContentType
        );

        pin_clock(&engine, u64::MAX);
        assert_eq!(
            engine
                .upload_blossom(request(url, b"overflow".to_vec()))
                .unwrap_err(),
            BlossomUploadError::AuthorizationWindow {
                created_at_secs: u64::MAX,
                lifetime_secs: AUTHORIZATION_LIFETIME.as_secs()
            }
        );

        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        engine.shutdown();
    }

    /// The engine is closed: the operation refuses rather than reaching a
    /// `Handle` that is no longer there.
    #[test]
    fn a_closed_engine_refuses_the_upload_rather_than_racing_its_own_teardown() {
        let engine = test_engine(true);
        add_active_account(&engine, 24);
        engine.shutdown();
        assert_eq!(
            engine
                .upload_blossom(request(
                    "https://blossom.example".to_string(),
                    b"closed".to_vec()
                ))
                .unwrap_err(),
            BlossomUploadError::EngineClosed
        );
    }

    /// Signer faults and CLOCK faults stay distinct: a device whose clock
    /// moved is not a signer that misbehaved.
    #[test]
    fn signer_failures_and_clock_failures_remain_distinct() {
        for (error, expected) in [
            (
                crate::SignerError::Unavailable,
                BlossomUploadError::SignerUnavailable {
                    reason: "signer unavailable".to_string(),
                },
            ),
            (
                crate::SignerError::Rejected("user said no".to_string()),
                BlossomUploadError::SignerRejected {
                    reason: "user said no".to_string(),
                },
            ),
            (
                crate::SignerError::InvalidResponse("forged".to_string()),
                BlossomUploadError::InvalidSignerOutput {
                    reason: "forged".to_string(),
                },
            ),
        ] {
            let engine = test_engine(true);
            let keys = Keys::generate();
            engine
                .add_signer(ErrorSigner {
                    public_key: keys.public_key(),
                    error,
                })
                .unwrap();
            engine.set_active_account(Some(keys.public_key())).unwrap();
            pin_clock(&engine, 1_000);
            assert_eq!(
                engine
                    .upload_blossom(request(
                        "https://blossom.example".to_string(),
                        b"signer refusal".to_vec(),
                    ))
                    .unwrap()
                    .recv()
                    .unwrap_err(),
                expected
            );
            engine.shutdown();
        }

        // A signer that takes longer than the authorization window: the
        // authorization is refused BEFORE the request, not after it.
        let engine = test_engine(true);
        let (keys, pending, _, _) = pending_signer(&engine);
        let clock = pin_clock(&engine, 1_000);
        let operation = engine
            .upload_blossom(request(
                "https://blossom.example".to_string(),
                b"expired".to_vec(),
            ))
            .unwrap();
        pending.wait_until_present();
        let expiration = 1_000 + AUTHORIZATION_LIFETIME.as_secs();
        clock.set(Timestamp::from(expiration));
        complete_pending(&keys, &pending);
        assert_eq!(
            operation.recv().unwrap_err(),
            BlossomUploadError::AuthorizationExpired {
                expiration_secs: expiration,
                now_secs: expiration
            }
        );
        engine.shutdown();

        // The same seam with the clock moved BACKWARD reports a clock fact,
        // not a signer fault.
        let engine = test_engine(true);
        let (keys, pending, _, _) = pending_signer(&engine);
        let clock = pin_clock(&engine, 5_000);
        let operation = engine
            .upload_blossom(request(
                "https://blossom.example".to_string(),
                b"backward".to_vec(),
            ))
            .unwrap();
        pending.wait_until_present();
        clock.set(Timestamp::from(4_000));
        complete_pending(&keys, &pending);
        assert_eq!(
            operation.recv().unwrap_err(),
            BlossomUploadError::ClockMovedBackward {
                created_at_secs: 5_000,
                now_secs: 4_000
            }
        );
        engine.shutdown();
    }

    /// Cancellation and shutdown during SIGNING wake the operation exactly
    /// once and withdraw the signer request exactly once.
    #[test]
    fn cancellation_and_shutdown_wake_pending_signers_exactly_once() {
        for shutdown in [false, true] {
            let engine = test_engine(true);
            let (_keys, pending, cancellations, _) = pending_signer(&engine);
            pin_clock(&engine, 1_000);
            let operation = engine
                .upload_blossom(request(
                    "https://blossom.example".to_string(),
                    b"pending".to_vec(),
                ))
                .unwrap();
            pending.wait_until_present();
            if shutdown {
                engine.shutdown();
            } else {
                let cancel = operation.cancel_handle();
                cancel.cancel();
                cancel.cancel();
            }
            assert_eq!(operation.recv().unwrap_err(), BlossomUploadError::Cancelled);
            cancellations.wait_for_one();
            if !shutdown {
                engine.shutdown();
            }
        }
    }

    /// Cancellation and shutdown AFTER the request was transmitted report the
    /// observation gap: the local operation stopped, and NMP claims nothing
    /// about what the remote did with bytes it has already seen.
    #[test]
    fn cancellation_and_shutdown_during_transmitted_http_report_observation_gap() {
        for shutdown in [false, true] {
            let bytes = if shutdown {
                b"shutdown during HTTP".to_vec()
            } else {
                b"cancel during HTTP".to_vec()
            };
            let hash = Sha256Hash::of(&bytes);
            let mut server = spawn_server(
                "200 OK",
                response_descriptor(hash, bytes.len(), "application/pdf"),
                true,
            );
            let engine = test_engine(true);
            add_active_account(&engine, if shutdown { 21 } else { 20 });
            pin_clock(&engine, 1_000);
            let operation = engine
                .upload_blossom(request(server.url.clone(), bytes.clone()))
                .unwrap();
            let captured = server
                .captured
                .recv_timeout(Duration::from_secs(2))
                .expect("HTTP request must reach the gated server");
            assert_eq!(captured.body, bytes, "the remote observed exact bytes");
            if shutdown {
                engine.shutdown();
            } else {
                operation.cancel_handle().cancel();
            }
            assert_eq!(
                operation.recv().unwrap_err(),
                BlossomUploadError::Cancelled,
                "local cancellation cannot claim whether the remote stored transmitted bytes"
            );
            server.release.take().unwrap().send(()).unwrap();
            if !shutdown {
                engine.shutdown();
            }
        }
    }

    /// The hardened transport's own protections survive the semantic wrapper:
    /// redirects, auth refusals, the response bound and the exact-hash gate
    /// all cross as their own variants rather than as one string.
    #[test]
    fn transport_and_integrity_protections_cross_the_semantic_operation() {
        let bytes = b"transport taxonomy".to_vec();
        let other = Sha256Hash::of(b"other");
        let cases = [
            (
                "302 Found",
                Vec::new(),
                BlossomUploadError::RedirectRefused { status: 302 },
            ),
            (
                "401 Unauthorized",
                b"no".to_vec(),
                BlossomUploadError::AuthRejected {
                    status: 401,
                    reason: None,
                },
            ),
            (
                "503 Service Unavailable",
                b"later".to_vec(),
                BlossomUploadError::ServerError {
                    status: 503,
                    reason: None,
                },
            ),
            (
                "200 OK",
                response_descriptor(other, bytes.len(), "application/pdf"),
                BlossomUploadError::Sha256Mismatch {
                    expected_sha256_hex: Sha256Hash::of(&bytes).to_hex(),
                    returned_sha256_hex: other.to_hex(),
                },
            ),
        ];

        for (index, (status, response, expected)) in cases.into_iter().enumerate() {
            let server = spawn_server(status, response, false);
            let engine = test_engine(true);
            add_active_account(&engine, 30 + index as u8);
            pin_clock(&engine, 1_000);
            assert_eq!(
                engine
                    .upload_blossom(request(server.url.clone(), bytes.clone()))
                    .unwrap()
                    .recv()
                    .unwrap_err(),
                expected
            );
            engine.shutdown();
        }

        let oversized = spawn_server("200 OK", vec![b'x'; 65_537], false);
        let engine = test_engine(true);
        add_active_account(&engine, 40);
        pin_clock(&engine, 1_000);
        assert_eq!(
            engine
                .upload_blossom(request(oversized.url.clone(), bytes.clone()))
                .unwrap()
                .recv()
                .unwrap_err(),
            BlossomUploadError::ResponseTooLarge {
                limit_bytes: 65_536
            }
        );
        engine.shutdown();
    }

    /// The operator's local-host opt-in is the SAME decision for a Blossom
    /// server as for a relay: an engine that did not opt in refuses loopback
    /// before any socket I/O.
    #[test]
    fn local_host_admission_follows_the_engine_operator_decision() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let engine = test_engine(false);
        add_active_account(&engine, 41);
        pin_clock(&engine, 1_000);
        assert_eq!(
            engine
                .upload_blossom(request(url, b"loopback".to_vec()))
                .unwrap()
                .recv()
                .unwrap_err(),
            BlossomUploadError::LocalHostNotAdmitted {
                host: "127.0.0.1".to_string()
            }
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        engine.shutdown();
    }
}
