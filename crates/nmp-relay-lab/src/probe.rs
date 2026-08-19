//! Is that port refused, or is it a black hole? Answered by errno.
//!
//! A probe that returns a boolean cannot tell the difference, and the
//! difference is the whole of the question. `ECONNREFUSED` means something is
//! there and said no -- it arrives in microseconds and is a definite answer.
//! A dropped SYN means nobody answered at all, is indistinguishable from a
//! slow network, and only ever "resolves" by a timeout the prober chose.
//!
//! A scenario that asserts "the relay is down" by waiting for a timeout is
//! asserting its own patience. This asserts the kernel's answer.

use std::io;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// What the kernel said about a connect attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortVerdict {
    /// The connection completed.
    Open,
    /// `ECONNREFUSED`: the host is reachable and nothing is listening. A
    /// DEFINITE negative -- something answered.
    Refused,
    /// `EHOSTUNREACH`/`ENETUNREACH`: routing said no.
    Unreachable,
    /// Nothing answered within the budget. Deliberately NOT called "closed":
    /// a black hole and a slow network are the same observation, and the only
    /// honest thing to say is that the wait ran out.
    NoAnswer { waited: Duration },
    /// Anything else, kept as itself rather than flattened.
    Other { kind: io::ErrorKind, message: String },
}

impl PortVerdict {
    /// True only for [`Self::Refused`] -- a definite, kernel-supplied "no".
    ///
    /// [`Self::NoAnswer`] is deliberately excluded. Treating a timeout as a
    /// refusal is how a scenario ends up asserting that its own patience ran
    /// out rather than that the port is shut.
    #[must_use]
    pub fn is_definitely_shut(&self) -> bool {
        matches!(self, Self::Refused)
    }
}

/// Connect once and report what the kernel said.
///
/// Blocking and bounded: `TcpStream::connect_timeout` reports `ECONNREFUSED`
/// immediately rather than waiting out the budget, which is exactly the
/// distinction this exists to preserve.
#[must_use]
pub fn probe(addr: SocketAddr, budget: Duration) -> PortVerdict {
    let started = Instant::now();
    match std::net::TcpStream::connect_timeout(&addr, budget) {
        Ok(_) => PortVerdict::Open,
        Err(error) => match error.kind() {
            io::ErrorKind::ConnectionRefused => PortVerdict::Refused,
            io::ErrorKind::HostUnreachable | io::ErrorKind::NetworkUnreachable => {
                PortVerdict::Unreachable
            }
            io::ErrorKind::TimedOut => PortVerdict::NoAnswer {
                waited: started.elapsed(),
            },
            kind => PortVerdict::Other {
                kind,
                message: error.to_string(),
            },
        },
    }
}

/// [`probe`], off the async runtime's worker threads.
pub async fn probe_async(addr: SocketAddr, budget: Duration) -> PortVerdict {
    tokio::task::spawn_blocking(move || probe(addr, budget))
        .await
        .expect("the probe task does not panic")
}

/// An address on RFC 5737 TEST-NET-1, which is reserved and non-routable:
/// SYNs to it are dropped rather than refused. The control every
/// "refused" assertion needs -- without it, a probe that returns `Refused`
/// for everything passes.
#[must_use]
pub fn black_hole() -> SocketAddr {
    SocketAddr::from(([192, 0, 2, 1], 9))
}
