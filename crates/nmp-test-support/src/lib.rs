//! Shared test-only fixtures and process seams for NMP integration tests.
//!
//! owns the NIP-19 reference corpus every parity surface reads; [`relays`]
//! owns the in-process scripted relay scenarios are built from; and
//! [`ConnectionOwner`] gives reconnect tests explicit ownership of every TCP
//! connection accepted on a relay's public address. Its async shutdown does
//! not return until the listener and all accepted sockets have been dropped.
//!
//! It also answers NIP-11 on the same address the relay serves websockets on
//! ([`ConnectionOwner::bind_with_tap_and_document`]), exactly as a real relay
//! does, so an acceptance test can drive the engine's REAL document
//! acquisition rather than injecting evidence behind its back.
//!
//! It can also TAP the client-to-relay byte direction ([`ClientTapFactory`]).
//! Nothing here decodes those bytes: the tap hands a caller the exact octets a
//! client put on the socket and the caller owns whatever framing sits above
//! them. This is the only observation point from which a test can see a REQ's
//! SUBSCRIPTION ID at all -- `nostr-relay-builder`'s `QueryPolicy` hook is
//! invoked once per FILTER, after the relay has already rewritten
//! `filter.limit`, and never sees the id.

#![forbid(unsafe_code)]

/// Shared NIP-19 reference corpus and its normalized expected values.
/// In-process relay with externally observable contact/query/write facts.
pub mod relays;

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::io::{copy_bidirectional, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::{JoinHandle, JoinSet};

/// Observer of the bytes ONE client connection sent toward the upstream relay,
/// in arrival order. `&mut` on purpose: any framing decoder above this seam is
/// a stateful reassembler, and each connection needs its own.
pub type ClientTap = Box<dyn FnMut(&[u8]) + Send>;

/// Mints a FRESH [`ClientTap`] per accepted connection. One shared closure
/// would interleave two connections' byte streams into a single reassembler
/// and corrupt both -- and a reconnect test accepts a second connection by
/// construction.
pub type ClientTapFactory = Arc<dyn Fn() -> ClientTap + Send + Sync>;

/// A TCP forwarding boundary that explicitly owns its public listener and
/// every accepted connection.
///
/// Reconnect tests put this in front of an in-process relay. Calling
/// [`Self::shutdown`] severs the client-facing sockets and releases the public
/// address before it returns, independently of the upstream relay's own
/// listener/session teardown semantics.
pub struct ConnectionOwner {
    local_addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<io::Result<()>>>,
}

impl ConnectionOwner {
    /// Bind `local_addr` and forward each accepted TCP stream to `upstream`.
    pub async fn bind(local_addr: SocketAddr, upstream: SocketAddr) -> io::Result<Self> {
        Self::bind_with_tap(local_addr, upstream, None).await
    }

    /// [`Self::bind`], plus a per-connection observer of every byte the client
    /// sends toward `upstream`. The tap never sees the relay-to-client
    /// direction and never alters the stream -- it is a pure copy of what was
    /// already going to be forwarded.
    pub async fn bind_with_tap(
        local_addr: SocketAddr,
        upstream: SocketAddr,
        tap: Option<ClientTapFactory>,
    ) -> io::Result<Self> {
        Self::bind_with_tap_and_document(local_addr, upstream, tap, None).await
    }

    /// [`Self::bind_with_tap`], plus the NIP-11 document this address serves
    /// over plain HTTP.
    ///
    /// A real relay answers both protocols on ONE address: a websocket
    /// upgrade becomes a relay session, and an ordinary `GET` with
    /// `Accept: application/nostr+json` returns the relay's own information
    /// document. `nostr-relay-builder`'s `LocalRelay` speaks only the first
    /// half, so the boundary that already owns this address answers the
    /// second -- which is what lets an acceptance test drive the REAL
    /// acquisition path (HTTP fetch, parse, capability evidence, planning)
    /// rather than injecting a document behind the engine's back.
    ///
    /// `None` means this relay publishes NO document, and the HTTP request
    /// is answered `404`. That is not a degenerate case: two of the eight
    /// major public relays measured for issue #931 publish nothing at all.
    pub async fn bind_with_tap_and_document(
        local_addr: SocketAddr,
        upstream: SocketAddr,
        tap: Option<ClientTapFactory>,
        relay_document: Option<String>,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(local_addr).await?;
        let local_addr = listener.local_addr()?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(run(
            listener,
            upstream,
            tap,
            Arc::new(relay_document),
            shutdown_rx,
        ));
        Ok(Self {
            local_addr,
            shutdown_tx: Some(shutdown_tx),
            task: Some(task),
        })
    }

    /// The client-facing address owned by this boundary.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Drop the public listener and every accepted socket, and wait until the
    /// task that owned them has completed.
    pub async fn shutdown(mut self) -> io::Result<()> {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
        if let Some(task) = self.task.take() {
            task.await.map_err(io::Error::other)??;
        }
        Ok(())
    }
}

impl Drop for ConnectionOwner {
    fn drop(&mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// A `TcpStream` whose READ side (the client-to-relay direction) is copied
/// into a [`ClientTap`] as it is forwarded. Transparent otherwise: every byte
/// still reaches the upstream relay unchanged, in the same order.
struct Tapped {
    inner: TcpStream,
    tap: Option<ClientTap>,
}

impl AsyncRead for Tapped {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let poll = Pin::new(&mut this.inner).poll_read(cx, buf);
        if matches!(poll, Poll::Ready(Ok(()))) {
            if let Some(tap) = this.tap.as_mut() {
                let filled = buf.filled();
                if filled.len() > before {
                    tap(&filled[before..]);
                }
            }
        }
        poll
    }
}

impl AsyncWrite for Tapped {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// How long one connection may take to reveal whether it is a websocket
/// upgrade or a plain HTTP request. A client that has said nothing by then is
/// forwarded upstream unchanged -- the pre-existing behaviour for every
/// connection, and the only safe default, since only the CLIENT speaks first
/// in both protocols.
const PROTOCOL_SNIFF_TIMEOUT: Duration = Duration::from_millis(250);

/// Peek (never consume) the start of `stream` until the request headers are
/// complete or the sniff window closes. Returns whatever was visible.
///
/// Peeking rather than reading is what keeps the websocket path byte-for-byte
/// what it always was: the handshake is still forwarded upstream in full, by
/// the same `copy_bidirectional`, and the tap still sees exactly the bytes the
/// client sent.
async fn peek_request_head(stream: &TcpStream) -> Vec<u8> {
    let deadline = Instant::now() + PROTOCOL_SNIFF_TIMEOUT;
    let mut buffer = vec![0_u8; 2048];
    loop {
        match stream.peek(&mut buffer).await {
            Ok(0) | Err(_) => return Vec::new(),
            Ok(read) => {
                let head = &buffer[..read];
                if head.windows(4).any(|window| window == b"\r\n\r\n") || read == buffer.len() {
                    return head.to_vec();
                }
            }
        }
        if Instant::now() >= deadline {
            let read = stream.peek(&mut buffer).await.unwrap_or(0);
            return buffer[..read].to_vec();
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// True iff this request head asks to become a websocket. Everything else
/// that starts with an HTTP method is an ordinary request -- in practice the
/// NIP-11 `GET`.
fn is_websocket_upgrade(head: &[u8]) -> bool {
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();
    text.contains("upgrade: websocket") || text.contains("sec-websocket-key")
}

fn looks_like_http_request(head: &[u8]) -> bool {
    head.starts_with(b"GET ") || head.starts_with(b"HEAD ") || head.starts_with(b"OPTIONS ")
}

fn request_head_len(head: &[u8]) -> Option<usize> {
    head.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

/// Answer one plain HTTP request with `document`, or `404` when this relay
/// publishes none. Redirects, proxies and retries are all disabled in the
/// engine's own fetcher, so exactly one clean response is what it needs.
async fn answer_relay_document(
    mut stream: TcpStream,
    request_head_len: usize,
    document: Option<&String>,
) -> io::Result<()> {
    // `peek_request_head` deliberately leaves the request untouched so the
    // websocket path can remain a byte-for-byte proxy. The plain-HTTP path
    // owns the connection instead, and must consume the request before
    // closing it: dropping a TCP socket with unread receive bytes produces a
    // reset on Darwin, so an otherwise complete NIP-11 response is reported
    // to the client as `ConnectionReset`.
    let mut request_head = vec![0; request_head_len];
    stream.read_exact(&mut request_head).await?;

    let response = match document {
        Some(body) => format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/nostr+json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        ),
        None => "HTTP/1.1 404 Not Found\r\n\
                 Content-Length: 0\r\n\
                 Connection: close\r\n\r\n"
            .to_string(),
    };
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    stream.shutdown().await
}

async fn run(
    listener: TcpListener,
    upstream: SocketAddr,
    tap: Option<ClientTapFactory>,
    relay_document: Arc<Option<String>>,
    mut shutdown_rx: oneshot::Receiver<()>,
) -> io::Result<()> {
    let mut connections = JoinSet::new();

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown_rx => break,
            result = listener.accept() => {
                let (downstream, _) = result?;
                let tap = tap.clone();
                let relay_document = Arc::clone(&relay_document);
                connections.spawn(async move {
                    let head = peek_request_head(&downstream).await;
                    // A NIP-11 fetch is answered HERE and never reaches the
                    // relay -- nor the client tap, which decodes websocket
                    // frames and would be poisoned by an HTTP request, nor
                    // the wire log every `wait_wire_quiet` reads.
                    if looks_like_http_request(&head) && !is_websocket_upgrade(&head) {
                        if let Some(request_head_len) = request_head_len(&head) {
                            return answer_relay_document(
                                downstream,
                                request_head_len,
                                relay_document.as_ref().as_ref(),
                            )
                            .await;
                        }
                    }
                    let mut downstream = Tapped {
                        inner: downstream,
                        tap: tap.as_ref().map(|factory| factory()),
                    };
                    let mut upstream = TcpStream::connect(upstream).await?;
                    copy_bidirectional(&mut downstream, &mut upstream).await?;
                    Ok::<(), io::Error>(())
                });
            }
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if let Ok(Err(error)) = result {
                    if !matches!(error.kind(), io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe | io::ErrorKind::UnexpectedEof) {
                        return Err(error);
                    }
                }
            }
        }
    }

    drop(listener);
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    Ok(())
}

