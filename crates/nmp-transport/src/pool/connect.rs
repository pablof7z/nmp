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

use nmp_network_policy::{AdmittedHost, Declarer, DestinationPolicy, DestinationRefusal};
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
/// 256 KiB (`relay_information::MAX_RESPONSE_BYTES`); ordinary Nostr relay
/// traffic (EVENTs, up to NIP-45-style COUNT responses) has no legitimate
/// need for anything near a megabyte. This is a hard ceiling, not derived
/// from any relay's self-reported `max_message_length` — a hostile relay's
/// own advertisement is not a trustworthy input to size its own leash.
const MAX_INBOUND_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_INBOUND_FRAME_BYTES: usize = 1024 * 1024;

fn relay_websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_INBOUND_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_INBOUND_FRAME_BYTES))
}

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
///
/// `destination_policy` is the one pure admission owner (#885,
/// `PoolConfig::destination_policy` — the SAME policy `nmp-engine`'s
/// `RelayAdmissionPolicy` enforces before a discovered relay ever reaches
/// this pool). It matters here because the URL's host STRING having already
/// cleared admission proves nothing about what it resolves to: an ordinary-
/// looking domain can still answer with a loopback/RFC-1918/link-local
/// address (DNS-based SSRF), and a second lookup at retry time could answer
/// differently than the first (DNS rebind). [`connect_with_timeout`] admits
/// the complete fresh answer set and connects to the exact
/// [`std::net::SocketAddr`] values it just admitted — never re-resolving —
/// so there is no window between the check and the connect for a rebind to
/// exploit.
///
/// `declarer` carries the engine's provenance answer for this exact
/// destination down to the socket (#1251). The pool does not re-derive it and
/// could not: whose declaration named a relay is knowable only where the
/// declaration was read. Passing it is what keeps ONE answer to one question —
/// a routing layer that admits what the dial refuses is two owners of the same
/// property, and the visible symptom is a relay the app was told it has and
/// can never reach.
pub(super) fn open_relay_socket(
    relay_url: &str,
    destination_policy: &DestinationPolicy,
    declarer: Declarer,
) -> Result<RelaySocket, String> {
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

    let stream = connect_with_timeout(host, port, CONNECT_TIMEOUT, destination_policy, declarer)
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
        client_tls_with_config(request, stream, Some(relay_websocket_config()), None).map_err(
            |error| match error {
                HandshakeError::Failure(f) => f.to_string(),
                HandshakeError::Interrupted(_) => {
                    "handshake interrupted on blocking stream".to_string()
                }
            },
        )?;
    Ok(socket)
}

/// Resolve `(host, port)`, admit the COMPLETE answer set, and connect to the
/// first admitted candidate. The exact `SocketAddr` values checked are the
/// ones `TcpStream::connect_timeout` is then called against — never a
/// re-resolved one — so there is no TOCTOU window for a DNS answer to change
/// between the check and the connect (a rebind attack).
///
/// One unallowed local answer refuses the WHOLE set, including a mixed
/// public+local answer: the convenient public subset is never dialed, because
/// selecting it is exactly how rebinding protection gets defeated. The typed
/// refusal maps to [`std::io::ErrorKind::PermissionDenied`], distinguishable
/// from an ordinary connect failure
/// ([`std::io::ErrorKind::AddrNotAvailable`]/whatever the OS reported) so
/// callers/tests can tell "refused" apart from "unreachable".
fn connect_with_timeout(
    host: &str,
    port: u16,
    timeout: Duration,
    destination_policy: &DestinationPolicy,
    declarer: Declarer,
) -> std::io::Result<TcpStream> {
    let admitted_host = destination_policy
        .admit_host(host, declarer)
        .map_err(destination_refusal)?;
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|error| {
            std::io::Error::new(error.kind(), format!("resolve {host}:{port}: {error}"))
        })?
        .collect();
    let addrs = admit_socket_addresses(destination_policy, &admitted_host, addrs)?;
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
            "no admitted address accepted the connection",
        )
    }))
}

/// Apply the shared all-or-nothing answer-set policy before any dial. The
/// returned addresses are exactly the ones admitted — nothing is filtered
/// out, so a partially-admissible answer cannot be silently narrowed.
fn admit_socket_addresses(
    destination_policy: &DestinationPolicy,
    admitted_host: &AdmittedHost,
    addresses: Vec<SocketAddr>,
) -> std::io::Result<Vec<SocketAddr>> {
    destination_policy
        .admit_resolved(admitted_host, addresses.iter().map(SocketAddr::ip))
        .map_err(destination_refusal)?;
    Ok(addresses)
}

fn destination_refusal(refusal: DestinationRefusal) -> std::io::Error {
    let kind = if matches!(refusal, DestinationRefusal::NoResolvedAddresses { .. }) {
        std::io::ErrorKind::AddrNotAvailable
    } else {
        std::io::ErrorKind::PermissionDenied
    };
    std::io::Error::new(kind, refusal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmp_network_policy::OnionReachability;
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

    /// A black-holed address must fail inside the bound, never the OS
    /// default (~75s). RFC 5737 TEST-NET-1 (`192.0.2.1`) is reserved and
    /// non-routable: SYNs are dropped.
    #[test]
    fn connect_with_timeout_is_bounded_not_os_default() {
        let started = Instant::now();
        let result = connect_with_timeout(
            "192.0.2.1",
            9,
            Duration::from_secs(2),
            &DestinationPolicy::default(),
            Declarer::SomeoneElse,
        );
        let elapsed = started.elapsed();
        assert!(result.is_err());
        assert!(
            elapsed < Duration::from_secs(10),
            "connect took {elapsed:?}; bound is not in effect"
        );
    }

    /// issue #519 Fix 1 falsifier: a host resolving ONLY to loopback/private/
    /// link-local/unspecified/broadcast addresses is refused WITHOUT ever
    /// attempting a connect — the refusal is immediate (well under the dial
    /// timeout) and distinguishable from an ordinary connect failure.
    #[test]
    fn resolved_local_addresses_are_refused_without_dialing() {
        // Deliberately literal IP text only: `(host, port).to_socket_addrs()`
        // fast-paths a parseable IP without touching the OS resolver, so
        // this stays a hermetic, network-free test. The trailing-dot
        // literal falsifier (`ws://127.0.0.1.`) is pinned at the relay-URL
        // adapter level instead (`nmp_grammar::relay`), where the SAME string
        // would instead go through `nostr`/`url`'s host parser rather than
        // `getaddrinfo`.
        for (host, port) in [
            ("127.0.0.1", 7777),
            ("10.0.0.1", 7777),
            ("169.254.169.254", 80), // cloud metadata endpoint
            ("::1", 7777),
        ] {
            let started = Instant::now();
            let result = connect_with_timeout(
                host,
                port,
                Duration::from_secs(5),
                &DestinationPolicy::default(),
                Declarer::SomeoneElse,
            );
            let elapsed = started.elapsed();
            let error = result.expect_err(&format!("{host} must be refused"));
            assert_eq!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied,
                "{host} must be refused as local, not merely unreachable"
            );
            assert!(
                elapsed < Duration::from_secs(1),
                "{host} refusal took {elapsed:?}; it must never attempt a TCP connect"
            );
        }
    }

    /// The operator opt-in (`PoolConfig::destination_policy`, threaded from
    /// `nmp-engine`'s `RelayAdmissionPolicy`) must still let an intentional
    /// local relay's resolved address through — issue #519's "don't break
    /// the intentional local-relay path" requirement. A loopback listener
    /// stands in for a real relay: reaching `Ok(_)` proves the policy did
    /// not skip the opted-in address.
    #[test]
    fn opted_in_local_host_still_connects() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept_thread = std::thread::spawn(move || listener.accept().unwrap());

        let policy =
            DestinationPolicy::new(["127.0.0.1".to_string()], OnionReachability::Unreachable);
        let result = connect_with_timeout(
            "127.0.0.1",
            port,
            Duration::from_secs(5),
            &policy,
            Declarer::SomeoneElse,
        );
        assert!(
            result.is_ok(),
            "an opted-in local host must still connect: {result:?}"
        );
        accept_thread.join().unwrap();
    }

    /// #1251 falsifier at the DIAL site: the provenance answer the engine
    /// computed has to survive to the socket. With no allowlist at all, the
    /// exact loopback address the previous test needed an opt-in for connects
    /// purely because the engine said this destination was one we declared.
    /// If the dial re-derived admission from the address alone, this would be
    /// refused and the relay the app was told it has would be unreachable.
    #[test]
    fn our_own_declaration_reaches_a_local_relay_with_no_allowlist() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept_thread = std::thread::spawn(move || listener.accept().unwrap());

        let policy = DestinationPolicy::default();
        let refused = connect_with_timeout(
            "127.0.0.1",
            port,
            Duration::from_secs(5),
            &policy,
            Declarer::SomeoneElse,
        );
        assert_eq!(
            refused
                .expect_err("a stranger may not name loopback")
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );

        let ours = connect_with_timeout(
            "127.0.0.1",
            port,
            Duration::from_secs(5),
            &policy,
            Declarer::Ourselves,
        );
        assert!(
            ours.is_ok(),
            "our own declaration must reach our own network: {ours:?}"
        );
        accept_thread.join().unwrap();
    }

    /// #885 falsifier at the DIAL site: a mixed public+local DNS answer for a
    /// public-looking host refuses the whole set. No `SocketAddr` survives to
    /// `TcpStream::connect_timeout`, so the convenient public subset is never
    /// dialed (that narrowing is precisely how DNS rebinding wins).
    #[test]
    fn mixed_resolved_answer_yields_no_dialable_address() {
        let policy = DestinationPolicy::default();
        let host = policy
            .admit_host("relay.example.com", Declarer::SomeoneElse)
            .expect("a public-looking literal host is admitted");
        let mixed: Vec<SocketAddr> = vec![
            "8.8.8.8:443".parse().unwrap(),
            "127.0.0.1:443".parse().unwrap(),
        ];
        let error = admit_socket_addresses(&policy, &host, mixed)
            .expect_err("a mixed answer must be refused in full");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        let reported = error.to_string();
        assert!(
            reported.contains("8.8.8.8") && reported.contains("127.0.0.1"),
            "the refusal must name both the public and the local answers: {reported}"
        );
    }
}
