//! The group door's DECLARED shape, and the gate that enforces it.
//!
//! Split from [`super::groups`] because it answers a different KIND of
//! question. Everything there is about what a run did; everything here is
//! about what the door could ever do -- "no group write operation accepts a
//! relay" is not observable from any execution, because a scenario that never
//! passed a relay proves nothing about whether it could have. The only witness
//! for the absence of a parameter is the declaration itself, which is also
//! exactly what `scripts/check-nip29-ownership.sh` reads, so the scenarios and
//! the gate agree on their evidence by construction.
//!
//! #1033 split the door across two files with no `GroupOperations` trait left
//! anywhere: `crates/nmp/src/nip29/group.rs` declares `Group`'s inherent read
//! and write methods (`door` below), and `crates/nmp/src/nip29/mod.rs`
//! declares the `RelayScope` door an app narrows to a `Group` through
//! (`binding` below, repurposed from "the engine binding" to "the scope
//! door" -- the field name survives because every consuming `Then` step still
//! reads it as "the other half of the surface").

use std::collections::BTreeSet;

use nmp::nip29::GroupContextError;
use nmp_grammar::EventBuilder;
use nostr::{Kind, Tag};

use super::NmpWorld;

/// The shape of the group door, read off its own source.
///
/// Three claims in `features/groups/` are about the ABSENCE of a parameter or
/// a composer ("no group write operation accepts a relay", "the group exposes
/// no composer for kind 9"). Absence is not observable from a run: a scenario
/// that never passed a relay proves nothing about whether it could have. The
/// only witness is the door's own declaration, which is also exactly what
/// `scripts/check-nip29-ownership.sh` reads -- so the scenario and the gate
/// agree on their evidence by construction.
#[derive(Debug, Default, Clone)]
pub struct GroupSurface {
    /// Every `pub fn` signature declared on `Group`'s own inherent impl.
    pub write_signatures: Vec<String>,
    /// Every `pub fn` the NIP-29 composer module exports.
    pub composer_fns: Vec<String>,
    /// Every kind constant that module binds.
    pub composer_kinds: BTreeSet<u16>,
    /// The `Group` door's own source, above its test module.
    pub door: String,
    /// The `RelayScope` door's own source, above its test module.
    pub binding: String,
}

/// Repo-root-relative source, read at run time. The BDD crate always runs
/// from its own manifest directory, two levels below the workspace root.
fn workspace_source(relative: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("nmp-bdd: cannot read {}: {error}", path.display()))
}

/// Everything above `#[cfg(test)]` -- the shipped half of a module. A probe
/// or a fixture below that line is not part of the door, which is the same
/// cut the ownership gate's own `awk` makes.
fn shipped_half(source: &str) -> String {
    match source.find("#[cfg(test)]") {
        Some(at) => source[..at].to_string(),
        None => source.to_string(),
    }
}

impl NmpWorld {
    /// Hand the door a draft that carries its own `h` and return what it
    /// says. Answers a claim about the door rather than about a run: a
    /// scenario that publishes nothing still asserts the refusal is
    /// unconditional.
    ///
    /// Contextualization is checked BEFORE the publish door is ever reached
    /// (`Group::publish` calls `nmp_nip29::contextualize` first), so handing
    /// this to the real engine still answers the question the old pure-door
    /// call answered: no signature, no journal row, no receipt, and no relay
    /// contact -- see [`nmp::nip29::GroupPublishError::Context`].
    pub async fn door_refuses_a_caller_supplied_context(&mut self) -> GroupContextError {
        // The group value needs a bound host, and starting the world contacts
        // nothing on its own -- which the sibling identity scenario proves.
        self.ensure_started().await;
        let group = self.group_value(None);
        let group_id = self.group_host_group_id();
        let author = self.me_pubkey();
        let draft = EventBuilder::new(Kind::from(9u16)).tag(
            Tag::parse(["h", &group_id])
                .expect("nmp-bdd: a two-value fixture tag is well-formed"),
        );
        let engine = self
            .engine
            .as_ref()
            .expect("nmp-bdd: the engine must be started before probing the door");
        match group.publish(engine, author, draft) {
            Err(nmp::nip29::GroupPublishError::Context(error)) => error,
            Err(nmp::nip29::GroupPublishError::Engine(error)) => panic!(
                "the group door must refuse a caller-supplied h row before reaching the \
                 publish door, but the publish door itself refused instead: {error:?}"
            ),
            Ok(_) => panic!(
                "the group door must refuse a caller-supplied h row, but the publication \
                 was accepted"
            ),
        }
    }

    /// `When I inspect the group's read/write/operation surface` -- reads the
    /// door's source once and keeps the parsed facts for the `Then`s.
    pub fn inspect_group_surface(&self, which: &str) {
        assert!(
            matches!(which, "read" | "write" | "operation"),
            "nmp-bdd: {which:?} is not a face of the group door"
        );
        let surface = self.group_surface();
        assert!(
            !surface.write_signatures.is_empty() && !surface.composer_fns.is_empty(),
            "nmp-bdd: the group door could not be read off its own source"
        );
    }

    /// The door's declared shape, read fresh off its own source.
    ///
    /// #1033 deleted the `GroupOperations` extension trait: every read and
    /// write method the app calls is now INHERENT on `Group`
    /// (`crates/nmp/src/nip29/group.rs`, `door` below), narrowed from a
    /// `RelayScope` (`crates/nmp/src/nip29/mod.rs`, `binding` below). Both are
    /// read verbatim rather than compiled against, because the claims this
    /// surface answers are about the SOURCE TEXT declaring a parameter or a
    /// verb, not about anything a call site could exercise.
    pub fn group_surface(&self) -> GroupSurface {
        let door = shipped_half(&workspace_source("crates/nmp/src/nip29/group.rs"));
        let binding = shipped_half(&workspace_source("crates/nmp/src/nip29/mod.rs"));
        let composers = shipped_half(&workspace_source("crates/nmp-nip29/src/operations.rs"));

        let mut write_signatures = Vec::new();
        let mut current: Option<String> = None;
        for line in door.lines() {
            let line = line.trim();
            if line.starts_with("pub fn ") {
                current = Some(
                    line.strip_prefix("pub ")
                        .expect("checked starts_with pub fn ")
                        .to_string(),
                );
            } else if let Some(sig) = current.as_mut() {
                sig.push(' ');
                sig.push_str(line);
            }
            if let Some(sig) = current.as_ref() {
                if sig.ends_with('{') || sig.ends_with(';') {
                    write_signatures.push(current.take().expect("checked"));
                }
            }
        }

        let composer_fns = composers
            .lines()
            .filter_map(|line| line.trim().strip_prefix("pub fn "))
            .filter_map(|rest| rest.split('(').next())
            .map(str::to_string)
            .collect();
        let composer_kinds = composers
            .lines()
            .filter_map(|line| line.trim().strip_prefix("const "))
            .filter_map(|rest| rest.split_once(": u16 = "))
            .filter_map(|(_, value)| value.trim_end_matches(';').parse::<u16>().ok())
            .collect();

        GroupSurface {
            write_signatures,
            composer_fns,
            composer_kinds,
            door,
            binding,
        }
    }

    /// `When the NIP-29 ownership gate inspects the group publication path` --
    /// runs the real script, against the real tree.
    pub fn run_ownership_gate(&mut self) {
        self.gate_outcome = Some(run_gate());
    }

    /// What the gate said when the scenario ran it.
    pub fn gate_outcome(&self) -> (bool, String) {
        self.gate_outcome
            .clone()
            .expect("nmp-bdd: the ownership gate has not been run")
    }

    /// What the gate says about the tree RIGHT NOW -- used to prove the
    /// negative probe below put the tree back exactly as it found it.
    pub fn gate_outcome_now(&self) -> (bool, String) {
        run_gate()
    }

    /// Run the gate against a tree that DOES branch on the kind, then put the
    /// tree back. Without this the gate scenario would only prove that a
    /// clean tree passes, which every disabled gate also does.
    ///
    /// The probe goes ABOVE any test module because the gate's own `awk`
    /// stops scanning at `#[cfg(test)]` -- a probe below that line would
    /// prove the opposite of what it claims. It is a bare `.rs` file that no
    /// `mod` declares, so nothing compiles it; the gate globs the directory,
    /// which is the surface being tested.
    pub fn gate_rejects_a_kind_branch(&self) -> (bool, String) {
        let probe = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/nmp-nip29/src/kind_branch_probe.rs");
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        std::fs::write(
            &probe,
            "// nmp-bdd negative probe for the NIP-29 ownership gate. Written, \
             measured and deleted\n// by one step; if you are reading this in a \
             checkout, that step was killed mid-run.\npub fn privileges_chat(kind: \
             nostr::Kind) -> bool {\n    kind == nostr::Kind::from(9)\n}\n",
        )
        .expect("nmp-bdd: the negative probe must be writable");
        let _cleanup = Cleanup(probe);
        run_gate()
    }
}

/// `scripts/check-nip29-ownership.sh`, run as CI runs it.
fn run_gate() -> (bool, String) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = std::process::Command::new("bash")
        .arg("scripts/check-nip29-ownership.sh")
        .current_dir(&root)
        .output()
        .expect("nmp-bdd: the ownership gate must be runnable");
    let mut said = String::from_utf8_lossy(&output.stdout).to_string();
    said.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.success(), said)
}
