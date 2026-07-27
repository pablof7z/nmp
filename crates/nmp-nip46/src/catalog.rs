//! Provider-owned local NIP-46 discovery facts.
//!
//! Native code executes `canOpenURL` / `PackageManager` probes and launches
//! the selected URI. It does not infer provider identity from a shared scheme
//! and this optional package does not catalog unrelated signer protocols.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nip46SignerApp {
    pub id: &'static str,
    pub display_name: &'static str,
    /// Exact iOS probe URI passed to `UIApplication.canOpenURL`.
    pub ios_detection_uri: Option<&'static str>,
    /// Scheme used to launch a generated NIP-46 invitation for this app.
    pub nip46_launch_scheme: Option<&'static str>,
    /// Exact Android probe URI. Package filtering remains mandatory because
    /// multiple apps can resolve `nostrsigner:`.
    pub android_detection_uri: Option<&'static str>,
    pub android_package_id: Option<&'static str>,
}

const KNOWN: &[Nip46SignerApp] = &[Nip46SignerApp {
    id: "primal",
    display_name: "Primal",
    // Primal iOS owns both nostrconnect and primalconnect. The app-specific
    // scheme avoids a system chooser when Primal was tapped.
    ios_detection_uri: Some("primalconnect://probe"),
    nip46_launch_scheme: Some("primalconnect"),
    // `primal://` alone does not match Android's host-constrained filters.
    android_detection_uri: Some("primal://signer"),
    android_package_id: Some("net.primal.android"),
}];

#[must_use]
pub const fn known_nip46_signers() -> &'static [Nip46SignerApp] {
    KNOWN
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_and_launch_facts_are_not_conflated() {
        let primal = known_nip46_signers()
            .iter()
            .find(|app| app.id == "primal")
            .unwrap();
        assert_eq!(primal.android_detection_uri, Some("primal://signer"));
        assert_eq!(primal.nip46_launch_scheme, Some("primalconnect"));
        assert_eq!(primal.android_package_id, Some("net.primal.android"));
    }

    #[test]
    fn unrelated_signer_protocols_are_absent() {
        assert_eq!(known_nip46_signers().len(), 1);
        assert!(
            known_nip46_signers()
                .iter()
                .all(|app| app.id != "amber"
                    && app.android_detection_uri != Some("nostrsigner:probe"))
        );
    }
}
