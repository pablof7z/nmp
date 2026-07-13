//! Shared process-level seams for NMP integration tests.
//!
//! This crate is test infrastructure, not product API. In particular,
//! [`ConnectionOwner`] gives reconnect tests explicit ownership of every TCP
//! connection accepted on a relay's public address. Its async shutdown does
//! not return until the listener and all accepted sockets have been dropped.

#![forbid(unsafe_code)]

use std::io;
use std::net::SocketAddr;

use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::{JoinHandle, JoinSet};

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
        let listener = TcpListener::bind(local_addr).await?;
        let local_addr = listener.local_addr()?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(run(listener, upstream, shutdown_rx));
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

async fn run(
    listener: TcpListener,
    upstream: SocketAddr,
    mut shutdown_rx: oneshot::Receiver<()>,
) -> io::Result<()> {
    let mut connections = JoinSet::new();

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown_rx => break,
            result = listener.accept() => {
                let (mut downstream, _) = result?;
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
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

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
