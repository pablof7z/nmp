//! Bounded relay-socket dialing.
//!
//! HARVEST source: the old repo's `crates/nmp-network/src/relay_worker/connect.rs`.
//! The load-bearing lesson kept here is the *bounded* `TcpStream::connect_timeout`
//! plus bounded handshake read/write timeouts — the blocking
//! `tungstenite::connect` helper dials with an unbounded `TcpStream::connect`,
//! so a relay that accepts SYNs but never finishes the handshake (or a
//! black-holed route) would otherwise wedge the worker thread for the OS
//! connect default (~75s), which in turn wedges `Pool::close`/`shutdown`
//! teardown for the same relay.
//!
//! Simplification vs. the harvested source: the old repo additionally bounds
//! DNS resolution itself with a detached helper-thread deadline (a `getaddrinfo`
//! hang is a separate, rarer failure mode). That refinement is dropped here —
//! out of scope for A2's test surface (no falsifier exercises a stuck-DNS
//! relay) — `to_socket_addrs` runs directly, bounded only by the OS resolver.
//! Noted as a deviation, not a silent narrowing.

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Once;
use std::time::Duration;

use tungstenite::client::{uri_mode, IntoClientRequest};
use tungstenite::error::{Error as WsError, UrlError};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::stream::{MaybeTlsStream, Mode};
use tungstenite::{client_tls_with_config, HandshakeError};

/// Upper bound on the OS-level TCP connect + TLS/HTTP upgrade for one dial.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Ceiling on one inbound WebSocket message/frame (issue #519 Fix 2). The
/// prior `None` `WebSocketConfig` took tungstenite's defaults verbatim (64
/// MiB message / 16 MiB frame) — a malicious or compromised relay could push
/// that much per message, times every live relay worker, as an unbounded
/// memory-amplification lever. NIP-11 documents are deliberately capped at
/// 256 KiB (`nmp_nip11`'s `MAX_RESPONSE_BYTES`); ordinary Nostr relay
/// traffic (EVENTs, up to NIP-45-style COUNT responses) has no legitimate
/// need for anything near a megabyte. This is a hard ceiling, not derived
/// from any relay's self-reported `max_message_length` — a hostile relay's
/// own advertisement is not a trustworthy input to size its own leash.
const MAX_INBOUND_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_INBOUND_FRAME_BYTES: usize = 1024 * 1024;

/// Ceiling on one outbound frame, symmetric with the inbound ceilings above
/// and enforced at admission (`pool::worker::WorkerHandle`) rather than here
/// (issue #506). A frame larger than this can never be handed to a relay, so
/// refusing it while the caller still holds a typed outcome is the honest
/// answer; letting it reach [`MAX_OUTBOUND_BUFFER_BYTES`] below would make
/// every subsequent write fail with a refusal that no caller is waiting for.
pub(super) const MAX_OUTBOUND_FRAME_BYTES: usize = 1024 * 1024;

/// What tungstenite may hold for a socket whose peer has stopped reading.
///
/// Successful `socket.write` is not peer delivery: it means the bytes were
/// accepted into this buffer. tungstenite 0.29 defaults
/// `max_write_buffer_size` to `usize::MAX`, so on a stalled peer the buffer
/// was the sink every bound upstream of it drained into — the worker's own
/// deque could be capped and the growth would simply move here (issue #506).
///
/// Sized so exactly one largest legal frame always fits beside a full
/// [`OUTBOUND_WRITE_BUFFER_BYTES`] of already-buffered bytes. A frame is
/// therefore refused only while the buffer genuinely holds unflushed data,
/// which the worker resolves by flushing — never permanently, which it could
/// not.
const OUTBOUND_WRITE_BUFFER_BYTES: usize = 128 * 1024;
const MAX_OUTBOUND_BUFFER_BYTES: usize = MAX_OUTBOUND_FRAME_BYTES + OUTBOUND_WRITE_BUFFER_BYTES;

fn relay_websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_INBOUND_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_INBOUND_FRAME_BYTES))
        .write_buffer_size(OUTBOUND_WRITE_BUFFER_BYTES)
        .max_write_buffer_size(MAX_OUTBOUND_BUFFER_BYTES)
}

pub(super) type RelaySocket = tungstenite::WebSocket<MaybeTlsStream<TcpStream>>;

/// Failure from [`open_relay_socket`]. `status` is `Some` only when the
/// relay's WebSocket handshake actually completed an HTTP exchange and that
/// response carried a status — a genuine HTTP-level denial, per
/// `backoff::is_permanent_error`. Every other failure shape reaching this
/// type (DNS resolution, a bare TCP connect, a stalled TLS/HTTP upgrade, an
/// interrupted handshake) carries `status: None`, so it can never be
/// misclassified as permanent by matching digits in `message` — which may
/// embed the relay's own host and port (issue #1788).
#[derive(Debug)]
pub(super) struct ConnectFailure {
    pub(super) message: String,
    pub(super) status: Option<u16>,
}

impl ConnectFailure {
    /// A failure with no HTTP status — every path except a completed
    /// handshake response.
    fn no_status(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status: None,
        }
    }
}

/// Extract the HTTP status a WebSocket handshake's response actually
/// carried, when the error is that kind of error. `None` for every other
/// tungstenite error shape (I/O, protocol, capacity, TLS, ...) — none of
/// those are an HTTP-level denial and must never be misclassified as one.
pub(super) fn handshake_http_status(error: &tungstenite::Error) -> Option<u16> {
    match error {
        WsError::Http(response) => Some(response.status().as_u16()),
        _ => None,
    }
}

fn install_rustls_provider() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Dial `relay_url`, returning a ready WebSocket. Bounded end-to-end by
/// [`CONNECT_TIMEOUT`]: a stuck TCP connect or a stalled TLS/HTTP upgrade
/// fails fast rather than wedging the worker thread.
///
pub(super) fn open_relay_socket(relay_url: &str) -> Result<RelaySocket, ConnectFailure> {
    install_rustls_provider();

    let mut request = relay_url
        .into_client_request()
        .map_err(|error| ConnectFailure::no_status(error.to_string()))?;
    request.headers_mut().insert(
        "User-Agent",
        tungstenite::http::HeaderValue::from_static(concat!("nmp/", env!("CARGO_PKG_VERSION"))),
    );
    let uri = request.uri();
    let mode = uri_mode(uri).map_err(|error| ConnectFailure::no_status(error.to_string()))?;

    let host = uri
        .host()
        .ok_or_else(|| ConnectFailure::no_status(WsError::Url(UrlError::NoHostName).to_string()))?;
    let host = if let Some(stripped) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        stripped
    } else {
        host
    };
    let port = uri.port_u16().unwrap_or(match mode {
        Mode::Plain => 80,
        Mode::Tls => 443,
    });

    let stream = connect_with_timeout(host, port, CONNECT_TIMEOUT)
        .map_err(|error| ConnectFailure::no_status(format!("tcp connect {host}:{port}: {error}")))?;
    stream
        .set_nodelay(true)
        .map_err(|error| ConnectFailure::no_status(format!("set_nodelay: {error}")))?;
    // Bound the TLS + HTTP-upgrade handshake the same way: a relay that
    // completes the TCP handshake but stalls the upgrade would otherwise
    // wedge the blocking `client_tls_with_config` reads/writes indefinitely.
    // `RelayPoller` puts the socket into non-blocking mode afterward, so this
    // timeout does not leak into the steady state.
    stream
        .set_read_timeout(Some(CONNECT_TIMEOUT))
        .map_err(|error| ConnectFailure::no_status(format!("set handshake read timeout: {error}")))?;
    stream
        .set_write_timeout(Some(CONNECT_TIMEOUT))
        .map_err(|error| ConnectFailure::no_status(format!("set handshake write timeout: {error}")))?;

    let (socket, _response) =
        client_tls_with_config(request, stream, Some(relay_websocket_config()), None).map_err(
            |error| match error {
                // The only shape that can carry an HTTP status: the relay
                // completed the HTTP upgrade exchange and responded with a
                // rejection (e.g. 401/403). Preserve that status as a typed
                // value instead of discarding it into `message`.
                HandshakeError::Failure(f) => ConnectFailure {
                    status: handshake_http_status(&f),
                    message: f.to_string(),
                },
                HandshakeError::Interrupted(_) => {
                    ConnectFailure::no_status("handshake interrupted on blocking stream")
                }
            },
        )?;
    Ok(socket)
}

/// Resolve `(host, port)` through the platform resolver and connect to the
/// first candidate that accepts within the per-address timeout. Address class
/// is not an NMP policy: loopback, private, link-local, and `.onion` names are
/// attempted exactly like any other syntactically valid relay destination.
fn connect_with_timeout(host: &str, port: u16, timeout: Duration) -> std::io::Result<TcpStream> {
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|error| {
            std::io::Error::new(error.kind(), format!("resolve {host}:{port}: {error}"))
        })?
        .collect();
    let mut last_err: Option<std::io::Error> = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_err = Some(error),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "the platform resolver returned no address",
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// issue #519 Fix 2 falsifier: the socket config actually applied by
    /// [`open_relay_socket`] must carry finite ceilings, not tungstenite's
    /// `None`-config defaults (64 MiB message / 16 MiB frame) that made a
    /// malicious relay's inbound message size an unbounded memory-
    /// amplification lever.
    #[test]
    fn relay_websocket_config_bounds_message_and_frame_size() {
        let config = relay_websocket_config();
        assert_eq!(config.max_message_size, Some(MAX_INBOUND_MESSAGE_BYTES));
        assert_eq!(config.max_frame_size, Some(MAX_INBOUND_FRAME_BYTES));
        // Compile-time invariant: tighter than tungstenite's own 64 MiB default ceiling.
        const _: () = assert!(MAX_INBOUND_MESSAGE_BYTES < 64 * 1024 * 1024);
    }

    /// Issue #506 falsifier: the write side needs the same treatment. With
    /// tungstenite's `usize::MAX` default, a peer that stops reading turns
    /// this buffer into the sink every upstream bound drains into, so the
    /// worker's own envelope would bound nothing.
    #[test]
    fn relay_websocket_config_bounds_the_write_buffer() {
        let config = relay_websocket_config();
        assert_eq!(config.max_write_buffer_size, MAX_OUTBOUND_BUFFER_BYTES);
        assert_eq!(config.write_buffer_size, OUTBOUND_WRITE_BUFFER_BYTES);
        // Compile-time invariants. The first is tungstenite's own
        // construction requirement; the second is what makes a
        // `WriteBufferFull` refusal always transient — one largest legal
        // frame fits beside a completely full write buffer, so a refusal
        // always means "flush first", never "this frame can never be sent".
        const _: () = assert!(MAX_OUTBOUND_BUFFER_BYTES > OUTBOUND_WRITE_BUFFER_BYTES);
        const _: () = assert!(
            MAX_OUTBOUND_BUFFER_BYTES >= MAX_OUTBOUND_FRAME_BYTES + OUTBOUND_WRITE_BUFFER_BYTES
        );
    }

    /// A black-holed address must fail inside the bound, never the OS
    /// default (~75s). RFC 5737 TEST-NET-1 (`192.0.2.1`) is reserved and
    /// non-routable: SYNs are dropped.
    #[test]
    fn connect_with_timeout_is_bounded_not_os_default() {
        let started = Instant::now();
        let result = connect_with_timeout("192.0.2.1", 9, Duration::from_secs(2));
        let elapsed = started.elapsed();
        assert!(result.is_err());
        assert!(
            elapsed < Duration::from_secs(10),
            "connect took {elapsed:?}; bound is not in effect"
        );
    }

    /// Issue #1429 falsifier: loopback is an ordinary destination. No
    /// allowlist, provenance classification, or NMP-owned address admission
    /// participates before the TCP connect.
    #[test]
    fn loopback_connects_without_policy_or_opt_in() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept_thread = std::thread::spawn(move || listener.accept().unwrap());

        let result = connect_with_timeout("127.0.0.1", port, Duration::from_secs(5));
        assert!(
            result.is_ok(),
            "loopback is an ordinary relay destination: {result:?}"
        );
        accept_thread.join().unwrap();
    }

    /// Issue #1788 falsifier: a plain connection refusal to a relay on port
    /// 4031 (or any port/hostname whose digits happen to spell "403") is an
    /// ordinary transient failure, not an HTTP-level denial. `open_relay_socket`
    /// still renders its TCP-connect error through the exact
    /// `"tcp connect {host}:{port}: {error}"` format at this file's
    /// `connect_with_timeout` call, so `message` genuinely contains the
    /// substring "403" -- but a bare TCP-connect failure never reaches an
    /// HTTP response, so `status` must be `None`, and
    /// `backoff::is_permanent_error` must therefore say "not permanent"
    /// regardless of what `message` contains. Before the fix, this exact
    /// message was fed to a substring-matching classifier and misclassified
    /// as a permanent HTTP-level denial, retiring the relay for the process
    /// lifetime (`pool.rs`'s documented "never self-reopens").
    #[test]
    fn plain_refusal_to_port_4031_is_not_a_permanent_denial() {
        // Nothing listens here: the OS refuses the connection immediately.
        let failure = open_relay_socket("ws://127.0.0.1:4031")
            .err()
            .expect("nothing should be listening on 127.0.0.1:4031");
        assert!(
            failure.message.contains("403"),
            "test setup: expected the rendered message to embed the port's digits, got {:?}",
            failure.message
        );
        assert_eq!(
            failure.status, None,
            "a plain TCP connect refusal never reaches an HTTP response, so it must \
             carry no status, got message {:?}",
            failure.message
        );
        assert!(
            !crate::backoff::is_permanent_error(failure.status),
            "issue #1788: a plain TCP connect refusal to port 4031 must not be \
             classified as a permanent HTTP-level denial, got message {:?}",
            failure.message
        );
    }
}
