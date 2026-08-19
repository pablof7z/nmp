//! The canary application's engine-facing layer.
//!
//! A NIP-29 rooms client where the people in rooms are portable identities you
//! can follow off them. Rooms are the primary noun; people are the secondary
//! noun. That shape was chosen because it puts two contradicting routing
//! authorities inside one app -- a NIP-29 event is pinned to its host relay by
//! protocol, a kind:1 is routed by NIP-65 outbox to a discovered relay set --
//! and sometimes inside one composed draft.
//!
//! This crate is not the UI. It is the queries, the writes, and the view-model
//! shapes each surface needs, written against the supported public surface and
//! compiled against a real `nmp::Engine`.
//!
//! **The findings are the deliverable.** Every module below opens with "what we
//! wanted to write" and "what we wrote". [`findings`] is the same content as
//! ranked data, so the exerciser can print it and nothing has to be taken on
//! trust. Where a suspicion turned out to be false, the module says so with the
//! code that proves it -- `composer` on the event id and the optimistic row,
//! `feed` on shortfall evidence, `room` on NIP-29.
//!
//! ## What this crate depends on, and why that is evidence
//!
//! `nmp` plus one line per capability, as intended -- and two lines that are
//! neither. `serde_json` exists only to read a kind:0 profile, and `tokio`
//! exists only to await `nmp_nip29::GroupObservation::next`. Both are recorded
//! in [`findings`]; see this crate's `Cargo.toml`.
//!
//! Deliberately NOT depended on: `nostr` and `nmp-grammar`. Every place where
//! naming one of them would have been easier is a finding instead.
//!
//! ## The binary is a supervisor, not a test
//!
//! `src/bin/canary.rs` spawns and kills child processes, because five of its
//! scenarios cannot be expressed any other way. A restart is only a restart if
//! the writing process exited -- a second `Engine` over one store in one
//! address space still holds the redb pages, the allocator and every decoded
//! row. A crash is only a crash under SIGKILL, with no `shutdown` and no
//! `Drop`. Descriptors, threads and resident size are properties of a process.
//! "The process exited" and "teardown returned" are different signals. Two
//! processes contending for one store is not a function call.
//!
//! Scenarios: `surfaces`, `deletions`, `routing`, `restart`, `crash`,
//! `contend`, `teardown`, `findings`, `all` (default). See [`process`].

#![deny(unsafe_code)]

pub mod app;
pub mod authgate;
pub mod composer;
pub mod deletions;
pub mod feed;
pub mod findings;
pub mod gated_relay;
pub mod notifications;
pub mod people;
pub mod process;
pub mod profiles;
pub mod room;
pub mod routing;
pub mod rows;
pub mod thread;

pub use app::Canary;
pub use findings::{report, Finding, Weight, FINDINGS};
