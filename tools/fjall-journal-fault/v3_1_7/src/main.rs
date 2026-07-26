//! Fjall 3.1.7 release probe for #818.
//!
//! The probe body is shared verbatim with the other pinned releases so that
//! the fixture, the fault, and the recorded evidence cannot drift between
//! versions. This file exists only to bind that body to one exact release.
#[path = "../../shared/probe.rs"]
mod probe;

/// The release this binary is compiled against, echoed into the evidence so a
/// mislinked probe cannot be mistaken for the version it claims to be.
const FJALL_VERSION: &str = "3.1.7";

fn main() {
    probe::run(FJALL_VERSION);
}
