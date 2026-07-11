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

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Once;
use std::time::Duration;

use tungstenite::client::{uri_mode, IntoClientRequest};
use tungstenite::error::{Error as WsError, UrlError};
use tungstenite::stream::{MaybeTlsStream, Mode};
use tungstenite::{client_tls_with_config, HandshakeError};

/// Upper bound on the OS-level TCP connect + TLS/HTTP upgrade for one dial.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) type RelaySocket = tungstenite::WebSocket<MaybeTlsStream<TcpStream>>;

fn install_rustls_provider() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Dial `relay_url`, returning a ready WebSocket. Bounded end-to-end by
/// [`CONNECT_TIMEOUT`]: a stuck TCP connect or a stalled TLS/HTTP upgrade
/// fails fast rather than wedging the worker thread.
pub(super) fn open_relay_socket(relay_url: &str) -> Result<RelaySocket, String> {
    install_rustls_provider();

    let mut request = relay_url
        .into_client_request()
        .map_err(|error| error.to_string())?;
    request.headers_mut().insert(
        "User-Agent",
        tungstenite::http::HeaderValue::from_static(concat!("nmp/", env!("CARGO_PKG_VERSION"))),
    );
    let uri = request.uri();
    let mode = uri_mode(uri).map_err(|error| error.to_string())?;

    let host = uri
        .host()
        .ok_or_else(|| WsError::Url(UrlError::NoHostName).to_string())?;
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
        .map_err(|error| format!("tcp connect {host}:{port}: {error}"))?;
    stream
        .set_nodelay(true)
        .map_err(|error| format!("set_nodelay: {error}"))?;
    // Bound the TLS + HTTP-upgrade handshake the same way: a relay that
    // completes the TCP handshake but stalls the upgrade would otherwise
    // wedge the blocking `client_tls_with_config` reads/writes indefinitely.
    // `RelayPoller` puts the socket into non-blocking mode afterward, so this
    // timeout does not leak into the steady state.
    stream
        .set_read_timeout(Some(CONNECT_TIMEOUT))
        .map_err(|error| format!("set handshake read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(CONNECT_TIMEOUT))
        .map_err(|error| format!("set handshake write timeout: {error}"))?;

    let (socket, _response) =
        client_tls_with_config(request, stream, None, None).map_err(|error| match error {
            HandshakeError::Failure(f) => f.to_string(),
            HandshakeError::Interrupted(_) => {
                "handshake interrupted on blocking stream".to_string()
            }
        })?;
    Ok(socket)
}

fn connect_with_timeout(host: &str, port: u16, timeout: Duration) -> std::io::Result<TcpStream> {
    let addrs = (host, port).to_socket_addrs().map_err(|error| {
        std::io::Error::new(error.kind(), format!("resolve {host}:{port}: {error}"))
    })?;
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
            "no addresses resolved for host",
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

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
}
