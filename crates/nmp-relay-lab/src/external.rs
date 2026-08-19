//! A relay this workspace did not write, in a process this workspace does not
//! share.
//!
//! [`RelayLab`](crate::RelayLab) is scriptable and now durable, and it answers
//! nearly everything. What it cannot answer is the one question that is about
//! somebody else's code: **does NMP interoperate with a relay implementation
//! nobody here designed?** A fixture written alongside the client agrees with
//! the client by construction, including where both are wrong.
//!
//! So this launches a real third-party relay as a child process. It is
//! deliberately NOT a thin binary wrapper around this crate's own relay --
//! that shape satisfies "separate process" while remaining a fake, and it is
//! the exact trap this module exists to avoid. What runs here is an
//! unmodified upstream binary with its own SQLite store.
//!
//! # Availability
//!
//! The binary is discovered, never vendored:
//!
//! 1. `$NMP_RELAY_LAB_RELAY_BIN`, if set, is used verbatim.
//! 2. Otherwise `nostr-rs-relay` is looked up on `$PATH`.
//!
//! Install it with `cargo install nostr-rs-relay` (omit `--locked`: the
//! published lockfile pins `ahash 0.7`, whose `feature(stdsimd)` no longer
//! compiles, and an unlocked resolve picks a version that does).
//!
//! Everything here is behind the non-default `external-relay` feature. That
//! is a deliberate choice over a runtime skip: a test that silently no-ops
//! when a binary is missing is a green run that proves nothing, and a cargo
//! feature is explicit, greppable, and impossible to pass by accident.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::probe::{probe, PortVerdict};

/// Where the upstream relay binary is, if it is anywhere.
#[must_use]
pub fn discover() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("NMP_RELAY_LAB_RELAY_BIN") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("nostr-rs-relay"))
        .find(|candidate| candidate.is_file())
}

/// A third-party relay, running, in its own process.
pub struct ExternalRelay {
    child: Option<Child>,
    port: u16,
    data_dir: PathBuf,
    url: nostr::RelayUrl,
}

impl std::fmt::Debug for ExternalRelay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalRelay")
            .field("url", &self.url)
            .field("pid", &self.pid())
            .field("data_dir", &self.data_dir)
            .finish()
    }
}

impl ExternalRelay {
    /// Start one on an ephemeral port with a fresh data directory.
    ///
    /// Panics if no binary was found: reaching this function at all means a
    /// scenario asked for a real relay, and answering that with a silent
    /// no-op is the failure mode this whole module is arranged against.
    pub fn start() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "nmp-relay-lab-external-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        Self::start_in(&dir)
    }

    /// Start one whose durable store is `data_dir` -- reusing an existing one
    /// is how a restart scenario proves the store outlived the process.
    pub fn start_in(data_dir: &Path) -> Self {
        let port = free_port();
        Self::start_in_on_port(data_dir, port)
    }

    /// Start on a SPECIFIC port, so a relay that comes back comes back at the
    /// `RelayUrl` NMP already has open.
    pub fn start_in_on_port(data_dir: &Path, port: u16) -> Self {
        let binary = discover().unwrap_or_else(|| {
            panic!(
                "nmp-relay-lab: no external relay binary. Set \
                 $NMP_RELAY_LAB_RELAY_BIN, or `cargo install nostr-rs-relay` \
                 (without --locked)."
            )
        });
        std::fs::create_dir_all(data_dir).expect("the relay's data directory");

        let config = data_dir.join("config.toml");
        std::fs::write(
            &config,
            format!(
                "[info]\n\
                 relay_url = \"ws://127.0.0.1:{port}/\"\n\
                 name = \"relay-lab-external\"\n\
                 \n[database]\n\
                 engine = \"sqlite\"\n\
                 data_directory = \"{}\"\n\
                 \n[network]\n\
                 address = \"127.0.0.1\"\n\
                 port = {port}\n\
                 \n[limits]\n\
                 messages_per_sec = 0\n",
                data_dir.display()
            ),
        )
        .expect("the relay's config is writable");

        let child = Command::new(&binary)
            .arg("--config")
            .arg(&config)
            .arg("--db")
            .arg(data_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|error| {
                panic!("nmp-relay-lab: cannot spawn {}: {error}", binary.display())
            });

        let url = nostr::RelayUrl::parse(&format!("ws://127.0.0.1:{port}"))
            .expect("the relay URL parses");
        let relay = Self {
            child: Some(child),
            port,
            data_dir: data_dir.to_path_buf(),
            url,
        };
        assert!(
            relay.wait_listening(Duration::from_secs(20)),
            "the external relay never started listening on {port}"
        );
        relay
    }

    #[must_use]
    pub fn url(&self) -> &nostr::RelayUrl {
        &self.url
    }

    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The relay's own durable directory. A sidecar reads the SQLite file
    /// here while the relay is down.
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        format!("127.0.0.1:{}", self.port)
            .parse()
            .expect("loopback address")
    }

    fn wait_listening(&self, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            if probe(self.addr(), Duration::from_millis(200)) == PortVerdict::Open {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// SIGKILL it and reap it. No shutdown, no flush, no unwinding -- what a
    /// crash actually is.
    ///
    /// Returns once the port is refused, so a caller that rebinds it next
    /// cannot race the kernel releasing the listener.
    pub fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if probe(self.addr(), Duration::from_millis(200)).is_definitely_shut() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for ExternalRelay {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn free_port() -> u16 {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is available");
    listener.local_addr().expect("local_addr").port()
}
