//! A relay's DURABLE contents: a file, not a `Vec` behind an `Arc`.
//!
//! Every "the relay comes back" fixture this replaces rebuilt an empty relay.
//! `ScriptedRelay::start_on_port` constructed a fresh builder with a brand-new
//! in-memory database, so a relay that came back had forgotten everything and
//! **"the relay gained events while the client was disconnected" was not a
//! sentence the Rust tree could say** -- which is the assertion an offline
//! scenario is entirely built around.
//!
//! A rebind is not a restart. What makes one is a store that outlives the
//! process that served it, and that is what this is: newline-delimited JSON,
//! one signed event per line, opened by path.
//!
//! Two consequences fall out for free, and both are requirements rather than
//! bonuses:
//!
//! - **A sidecar can write to it during an outage.** [`RelayStore::append`]
//!   takes a path, not a handle, so a second writer -- another test, another
//!   thread, another PROCESS -- adds events to a relay that is not running.
//!   That is the "gained events while dead" half.
//! - **A sidecar can read it during an outage**, so a scenario can assert
//!   what the relay durably holds at a moment when nothing is serving it.

use std::path::{Path, PathBuf};

use nostr::{Event, JsonUtil};

/// A relay's durable contents on disk.
///
/// Newline-delimited JSON on purpose: append is atomic enough for one writer
/// at a time, a human can read the file, and a sidecar in any language can
/// append a line. Nothing here is a database and nothing here should become
/// one -- the moment it needs indexes, the scenario wants a real relay.
#[derive(Debug, Clone)]
pub struct RelayStore {
    path: PathBuf,
}

impl RelayStore {
    #[must_use]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every event the store holds. A missing file is an empty store, never
    /// an error: a relay that has never been written to is an ordinary state.
    ///
    /// A line that will not parse is a FAULT and panics, rather than being
    /// skipped. A silently dropped event would make a durability scenario
    /// pass by losing exactly the thing it is asserting survived.
    #[must_use]
    pub fn read(&self) -> Vec<Event> {
        let Ok(contents) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                Event::from_json(line).unwrap_or_else(|error| {
                    panic!(
                        "nmp-relay-lab: {} holds a line that is not an event ({error}): {line}",
                        self.path.display()
                    )
                })
            })
            .collect()
    }

    /// Append events, creating the file if it does not exist.
    ///
    /// Takes `&self` and a path rather than a live relay, which is the whole
    /// point: this is callable while nothing is serving the store.
    pub fn append(&self, events: impl IntoIterator<Item = Event>) {
        use std::io::Write;
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .unwrap_or_else(|error| {
                panic!(
                    "nmp-relay-lab: cannot open {} for append: {error}",
                    self.path.display()
                )
            });
        for event in events {
            writeln!(file, "{}", event.as_json()).expect("a durable relay store stays writable");
        }
    }

    /// How many events the store holds, without decoding them.
    #[must_use]
    pub fn len(&self) -> usize {
        std::fs::read_to_string(&self.path)
            .map(|c| c.lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
