//! The `nmp-audit` workspace-wide kind-ownership audit
//! (`docs/design/routing-and-ownership.md` §4.2 layer 2,
//! `docs/design/routing-build-plan.md` §4). See `crates/nmp-audit/src/lib.rs`
//! for the crate-level summary of what this file proves.

use std::collections::BTreeSet;
use std::env;
use std::process::Command;

use nmp_ownership::{ClaimSet, KindClaim, KindScope, ModuleId};

/// One enrolled workspace entry: either a real module crate's `claims()`
/// table, or an explicit, documented declaration that the crate owns no
/// kinds (routing-and-ownership.md §3.2.1 -- contextual publication is not
/// kind ownership).
enum Enrollment {
    /// Owned rows rather than `&'static [KindClaim]`: enrolled crates
    /// export either a static table (`nmp-nip02`/`nmp-nip51`) or an owned
    /// `Vec` (`nmp-blossom`, #545), and the audit folds both identically.
    Claims(Vec<KindClaim>),
    DeclaresNoClaims {
        rationale: &'static str,
    },
}

/// The hand-maintained enrollment registry. Every workspace crate that
/// declares a NORMAL (non-dev, non-build) dependency on `nmp-ownership` MUST
/// have an entry here -- `enrollment_matches_cargo_metadata` below proves
/// the two sets are exactly equal, in both directions.
fn registry() -> Vec<(&'static str, Enrollment)> {
    vec![
        (
            "nmp-nip02",
            Enrollment::Claims(nmp_nip02::claims().to_vec()),
        ),
        (
            "nmp-nip51",
            Enrollment::Claims(nmp_nip51::claims().to_vec()),
        ),
        (
            "nmp-nip29",
            Enrollment::DeclaresNoClaims {
                rationale: "contextual publication is not kind ownership \
                    (routing-and-ownership.md §3.2.1); the crate deliberately \
                    exports no claims() -- see its lib.rs ownership_audit module",
            },
        ),
        ("nmp-blossom", Enrollment::Claims(nmp_blossom::claims())),
        ("nmp-nip68", Enrollment::Claims(nmp_nip68::claims())),
        (
            "nmp-media",
            Enrollment::DeclaresNoClaims {
                rationale: "composition/orchestration is not kind ownership \
                    (routing-and-ownership.md §3.2.1); the crate deliberately \
                    exports no claims() -- it wraps nmp-blossom (kind:24242) and \
                    nmp-nip68 (kind:20) artifacts without defining any, see its \
                    lib.rs ownership_audit module (#559)",
            },
        ),
    ]
}

/// Every `KindClaim` contributed by an enrolled `Claims(...)` entry,
/// flattened across the whole registry. `DeclaresNoClaims` entries
/// contribute nothing, by definition.
fn all_registered_claims() -> Vec<KindClaim> {
    registry()
        .into_iter()
        .filter_map(|(_, enrollment)| match enrollment {
            Enrollment::Claims(claims) => Some(claims),
            Enrollment::DeclaresNoClaims { .. } => None,
        })
        .flatten()
        .collect()
}

/// Whether `scope` shares at least one kind with `nmp_router`'s
/// `DiscoveryKinds` ({0, 3} ∪ 10000..=19999) -- the predicate
/// `discovery_ack` must track in both directions (routing-and-ownership.md
/// §4.2 layer 2, check (c)).
fn intersects_discovery(scope: &KindScope) -> bool {
    let dk = nmp_router::DiscoveryKinds::default();
    dk.0.iter().any(|k| scope.contains(*k))
}

/// The claims (from `claims`) whose `discovery_ack` does NOT match
/// `intersects_discovery(&claim.scope)` -- in either direction: an
/// unacknowledged discovery-kind claim, or a stale ack on a non-discovery
/// scope. A helper (rather than an inline assert loop) so the fixture
/// falsifiers below can drive the exact same predicate the real workspace
/// test enforces, without panicking the whole suite.
fn discovery_ack_violations<'a, I>(claims: I) -> Vec<&'a KindClaim>
where
    I: IntoIterator<Item = &'a KindClaim>,
{
    claims
        .into_iter()
        .filter(|claim| claim.discovery_ack != intersects_discovery(&claim.scope))
        .collect()
}

fn fixture_claim(
    owner: &'static str,
    scope: KindScope,
    exclusive: bool,
    discovery_ack: bool,
) -> KindClaim {
    KindClaim {
        owner: ModuleId::new(owner),
        scope,
        exclusive,
        route_policy: None,
        discovery_ack,
    }
}

/// The enrollment red-build property: derive, from `cargo metadata`, the
/// set of workspace package names (excluding `nmp-audit` itself) that
/// declare a NORMAL dependency on `nmp-ownership` (dependency `kind` ==
/// null; dev/build-dependency kinds are excluded), and assert it is
/// EXACTLY the registry's name set. A new claim-bearing module crate that
/// forgets to enroll fails here; a stale registry entry for a crate that no
/// longer depends on `nmp-ownership` also fails here.
#[test]
fn enrollment_matches_cargo_metadata() {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to spawn `cargo metadata`");
    assert!(
        output.status.success(),
        "`cargo metadata` failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("`cargo metadata` produced invalid JSON");
    let packages = metadata["packages"]
        .as_array()
        .expect("`cargo metadata` JSON has no `packages` array");

    let mut depends_on_ownership: BTreeSet<String> = BTreeSet::new();
    for package in packages {
        let name = package["name"]
            .as_str()
            .expect("package entry has no `name`")
            .to_string();
        if name == "nmp-audit" {
            // nmp-audit itself dev-depends on nmp-ownership to run this
            // very test -- that's not a claim-bearing module and must not
            // be asked to enroll itself.
            continue;
        }
        let dependencies = package["dependencies"]
            .as_array()
            .expect("package entry has no `dependencies` array");
        let has_normal_ownership_dep = dependencies
            .iter()
            .any(|dep| dep["name"].as_str() == Some("nmp-ownership") && dep["kind"].is_null());
        if has_normal_ownership_dep {
            depends_on_ownership.insert(name);
        }
    }

    let registered: BTreeSet<String> = registry()
        .into_iter()
        .map(|(name, _)| name.to_string())
        .collect();

    for name in &depends_on_ownership {
        assert!(
            registered.contains(name),
            "crate {name} depends on nmp-ownership but is not enrolled in \
             nmp-audit's registry -- add it to registry() AND to nmp-audit's \
             dev-dependencies"
        );
    }
    for name in &registered {
        assert!(
            depends_on_ownership.contains(name),
            "registry entry {name} is stale -- it is enrolled in nmp-audit's \
             registry() but `cargo metadata` shows no normal dependency on \
             nmp-ownership for that crate"
        );
    }
}

/// Every `DeclaresNoClaims` entry documents WHY the crate doesn't export
/// claims -- reads `rationale` so it isn't silently unused.
#[test]
fn declares_no_claims_entries_carry_a_nonempty_rationale() {
    for (name, enrollment) in registry() {
        if let Enrollment::DeclaresNoClaims { rationale } = enrollment {
            assert!(
                !rationale.is_empty(),
                "{name} declares no claims but its registry entry has an empty rationale"
            );
        }
    }
}

/// §4.2 layer 2 check (a): every enrolled module's claims fold together
/// without an exclusive-scope overlap, across the WHOLE workspace,
/// including modules no app currently links.
#[test]
fn workspace_claims_fold_without_exclusive_overlap() {
    if let Err(overlap) = ClaimSet::build(all_registered_claims()) {
        panic!("{overlap}");
    }
}

/// §4.2 layer 2 check (b): route authority ⊆ ownership -- a claim that
/// installs a `RoutePolicy` must be `exclusive` (a route override on a
/// shared/non-exclusive claim is drift, §4.3).
#[test]
fn route_authority_rides_exclusive_claims() {
    for claim in &all_registered_claims() {
        if claim.route_policy.is_some() {
            assert!(
                claim.exclusive,
                "claim owned by {} carries a route_policy but is not \
                 exclusive -- route authority must ride an exclusive claim \
                 (routing-and-ownership.md §4.3)",
                claim.owner
            );
        }
    }
}

/// §4.2 layer 2 check (c): a claim whose scope intersects `DiscoveryKinds`
/// must set `discovery_ack: true`, and a claim whose scope does NOT
/// intersect `DiscoveryKinds` must not set it -- checked in both
/// directions.
#[test]
fn discovery_kind_claims_are_consciously_acknowledged() {
    let claims = all_registered_claims();
    let violations = discovery_ack_violations(&claims);
    assert!(
        violations.is_empty(),
        "discovery_ack mismatch for: {}",
        violations
            .iter()
            .map(|c| format!(
                "{} (scope {:?}, discovery_ack={})",
                c.owner, c.scope, c.discovery_ack
            ))
            .collect::<Vec<_>>()
            .join(", ")
    );
}

// --- Fixture falsifiers (routing-build-plan.md §4.3/§4.4) ---
//
// Zero real module crates exist beyond nip02/nip51/nip29/blossom at this
// milestone, so the falsifiers below drive the SAME mechanisms
// (`ClaimSet::build`, `discovery_ack_violations`) against synthetic
// `fixture-*` module ids, per build-plan §4.4: "do not fake a real module --
// the fixtures live in the audit crate's test tree."

#[test]
fn two_exclusive_claims_on_one_kind_fail_the_fold() {
    let a = fixture_claim("fixture-a", KindScope::Kind(500), true, false);
    let b = fixture_claim("fixture-b", KindScope::Kind(500), true, false);
    let err =
        ClaimSet::build([a, b]).expect_err("two overlapping exclusive fixtures must fail the fold");
    assert_eq!(err.witness_kind, 500);
    let msg = err.to_string();
    assert!(
        msg.contains("fixture-a"),
        "message must name the first owner: {msg}"
    );
    assert!(
        msg.contains("fixture-b"),
        "message must name the second owner: {msg}"
    );
}

#[test]
fn exclusive_overlap_with_nonexclusive_claim_still_fails() {
    let a = fixture_claim("fixture-a", KindScope::Kind(501), true, false);
    let b = fixture_claim("fixture-b", KindScope::Kind(501), false, false);
    let err = ClaimSet::build([a, b])
        .expect_err("one exclusive side is enough to refuse an overlapping fold");
    assert_eq!(err.witness_kind, 501);
}

#[test]
fn deliberately_shared_nonexclusive_scopes_fold_ok() {
    let a = fixture_claim("fixture-a", KindScope::Kind(502), false, false);
    let b = fixture_claim("fixture-b", KindScope::Kind(502), false, false);
    ClaimSet::build([a, b])
        .expect("two non-exclusive overlapping claims are deliberate sharing, not drift (§4.1)");
}

#[test]
fn unacknowledged_discovery_claim_is_caught() {
    // Kind(3) and Kind(10050) both intersect DiscoveryKinds ({0, 3} ∪
    // 10000..=19999).
    assert!(intersects_discovery(&KindScope::Kind(3)));
    assert!(intersects_discovery(&KindScope::Kind(10050)));

    let unacked_kind3 = fixture_claim("fixture-a", KindScope::Kind(3), true, false);
    assert_eq!(discovery_ack_violations([&unacked_kind3]).len(), 1);

    let unacked_kind10050 = fixture_claim("fixture-b", KindScope::Kind(10050), true, false);
    assert_eq!(discovery_ack_violations([&unacked_kind10050]).len(), 1);
}

#[test]
fn stale_discovery_ack_is_caught() {
    // Kind(1) and Range(9000..=9030) (NIP-29-shaped) neither intersect
    // DiscoveryKinds.
    assert!(!intersects_discovery(&KindScope::Kind(1)));
    assert!(!intersects_discovery(&KindScope::Range(9000..=9030)));

    let stale_kind1 = fixture_claim("fixture-a", KindScope::Kind(1), true, true);
    assert_eq!(discovery_ack_violations([&stale_kind1]).len(), 1);

    let stale_range = fixture_claim("fixture-b", KindScope::Range(9000..=9030), true, true);
    assert_eq!(discovery_ack_violations([&stale_range]).len(), 1);
}
