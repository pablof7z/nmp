//! Shared test-only fixtures and process seams for NMP integration tests.
//!
//! This crate is test infrastructure, not product API. [`reference_fixtures`]
//! owns the NIP-19 reference corpus every parity surface reads; [`relays`]
//! owns the in-process scripted relay scenarios are built from; and
//! [`ConnectionOwner`] gives reconnect tests explicit ownership of every TCP
//! connection accepted on a relay's public address. Its async shutdown does
//! not return until the listener and all accepted sockets have been dropped.
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
pub mod reference_fixtures;
/// In-process relay with externally observable contact/query/write facts.
pub mod relays;

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{copy_bidirectional, AsyncRead, AsyncWrite, ReadBuf};
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
        let listener = TcpListener::bind(local_addr).await?;
        let local_addr = listener.local_addr()?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(run(listener, upstream, tap, shutdown_rx));
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

async fn run(
    listener: TcpListener,
    upstream: SocketAddr,
    tap: Option<ClientTapFactory>,
    mut shutdown_rx: oneshot::Receiver<()>,
) -> io::Result<()> {
    let mut connections = JoinSet::new();

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown_rx => break,
            result = listener.accept() => {
                let (downstream, _) = result?;
                let mut downstream = Tapped {
                    inner: downstream,
                    tap: tap.as_ref().map(|factory| factory()),
                };
                connections.spawn(async move {
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

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::Mutex;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// The tap must be a FAITHFUL copy of the client-to-upstream direction:
    /// every byte the client wrote, in order, and nothing the upstream wrote
    /// back. Anything less and a decoder above this seam would silently miss
    /// frames -- the failure mode that turns a red spec green.
    #[tokio::test]
    async fn the_tap_sees_every_client_byte_and_no_reply_byte() {
        let upstream = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind upstream");
        let upstream_addr = upstream.local_addr().expect("upstream address");
        let upstream_task = tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.expect("accept upstream");
            let mut seen = [0_u8; 5];
            stream
                .read_exact(&mut seen)
                .await
                .expect("read client bytes");
            stream.write_all(b"reply").await.expect("write reply");
            std::future::pending::<()>().await;
        });

        let tapped: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&tapped);
        let owner = ConnectionOwner::bind_with_tap(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            upstream_addr,
            Some(Arc::new(move || {
                let sink = Arc::clone(&sink);
                Box::new(move |bytes: &[u8]| {
                    sink.lock().expect("tap sink").extend_from_slice(bytes)
                })
            })),
        )
        .await
        .expect("bind tapping connection owner");

        let mut client = TcpStream::connect(owner.local_addr())
            .await
            .expect("connect through owner");
        // Written in two chunks: the tap must concatenate, not overwrite.
        client.write_all(b"he").await.expect("write first chunk");
        client.write_all(b"llo").await.expect("write second chunk");
        let mut reply = [0_u8; 5];
        client.read_exact(&mut reply).await.expect("read reply");
        assert_eq!(&reply, b"reply", "the proxy must still forward both ways");

        assert_eq!(
            tapped.lock().expect("tap sink").as_slice(),
            b"hello",
            "the tap must see exactly the client's bytes -- no reply bytes, nothing dropped"
        );

        owner.shutdown().await.expect("shutdown owner");
        upstream_task.abort();
    }

    #[tokio::test]
    async fn shutdown_closes_active_connections_and_releases_listener() {
        let upstream = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind upstream");
        let upstream_addr = upstream.local_addr().expect("upstream address");
        let upstream_task = tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.expect("accept upstream");
            let mut byte = [0_u8; 1];
            stream.read_exact(&mut byte).await.expect("read marker");
            stream.write_all(&byte).await.expect("echo marker");
            std::future::pending::<()>().await;
        });

        let owner = ConnectionOwner::bind(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            upstream_addr,
        )
        .await
        .expect("bind connection owner");
        let public_addr = owner.local_addr();
        let mut client = TcpStream::connect(public_addr)
            .await
            .expect("connect through owner");
        client.write_all(&[7]).await.expect("write marker");
        let mut echoed = [0_u8; 1];
        client.read_exact(&mut echoed).await.expect("read echo");
        assert_eq!(echoed, [7]);

        owner.shutdown().await.expect("shutdown owner");

        let read = tokio::time::timeout(Duration::from_secs(1), client.read(&mut echoed))
            .await
            .expect("owned connection must close within the bound");
        assert!(
            matches!(read, Ok(0) | Err(_)),
            "connection remained open: {read:?}"
        );
        TcpListener::bind(public_addr)
            .await
            .expect("owner must release public listener before returning");

        upstream_task.abort();
    }
}
