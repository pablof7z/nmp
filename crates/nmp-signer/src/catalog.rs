//! Rust-owned local-signer discovery facts.
//!
//! Native code executes `canOpenURL` / `PackageManager` probes and launches
//! the selected URI. It does not infer protocol or signer identity from a
//! shared scheme such as `nostrsigner:`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalSignerProtocol {
    Nip46,
    Nip55,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalSignerApp {
    pub id: &'static str,
    pub display_name: &'static str,
    pub protocols: &'static [LocalSignerProtocol],
    /// Exact iOS probe URI passed to `UIApplication.canOpenURL`.
    pub ios_detection_uri: Option<&'static str>,
    /// Scheme used to launch a generated NIP-46 invitation for this app.
    pub nip46_launch_scheme: Option<&'static str>,
    /// Exact Android probe URI. Package filtering remains mandatory because
    /// multiple apps can resolve `nostrsigner:`.
    pub android_detection_uri: Option<&'static str>,
    pub android_package_id: Option<&'static str>,
    /// Base ContentProvider authority; method suffixes are protocol facts,
    /// not package IDs.
    pub android_provider_authority: Option<&'static str>,
}

const NIP46_NIP55: &[LocalSignerProtocol] =
    &[LocalSignerProtocol::Nip46, LocalSignerProtocol::Nip55];
const NIP55: &[LocalSignerProtocol] = &[LocalSignerProtocol::Nip55];

const KNOWN: &[LocalSignerApp] = &[
    LocalSignerApp {
        id: "primal",
        display_name: "Primal",
        protocols: NIP46_NIP55,
        // Primal iOS owns both nostrconnect and primalconnect. The
        // app-specific scheme avoids a system chooser when Primal was tapped.
        ios_detection_uri: Some("primalconnect://probe"),
        nip46_launch_scheme: Some("primalconnect"),
        // `primal://` alone does not match Android's host-constrained filters.
        android_detection_uri: Some("primal://signer"),
        android_package_id: Some("net.primal.android"),
        android_provider_authority: Some("net.primal.android"),
    },
    LocalSignerApp {
        id: "amber",
        display_name: "Amber",
        protocols: NIP55,
        // Amber is Android-only; claiming an iOS nostrsigner handler creates
        // a false one-click option.
        ios_detection_uri: None,
        nip46_launch_scheme: None,
        android_detection_uri: Some("nostrsigner:probe"),
        android_package_id: Some("com.greenart7c3.nostrsigner"),
        android_provider_authority: Some("com.greenart7c3.nostrsigner"),
    },
];

#[must_use]
pub const fn known_local_signers() -> &'static [LocalSignerApp] {
    KNOWN
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_and_launch_facts_are_not_conflated() {
        let primal = known_local_signers()
            .iter()
            .find(|app| app.id == "primal")
            .unwrap();
        assert_eq!(primal.android_detection_uri, Some("primal://signer"));
        assert_eq!(primal.nip46_launch_scheme, Some("primalconnect"));
        assert_eq!(primal.android_package_id, Some("net.primal.android"));
        assert_eq!(
            primal.android_provider_authority,
            Some("net.primal.android")
        );
        assert!(primal.protocols.contains(&LocalSignerProtocol::Nip46));
        assert!(primal.protocols.contains(&LocalSignerProtocol::Nip55));
    }

    #[test]
    fn amber_is_android_nip55_only() {
        let amber = known_local_signers()
            .iter()
            .find(|app| app.id == "amber")
            .unwrap();
        assert_eq!(amber.ios_detection_uri, None);
        assert_eq!(amber.protocols, NIP55);
        assert_eq!(amber.android_detection_uri, Some("nostrsigner:probe"));
        assert!(amber.android_package_id.is_some());
    }

    #[test]
    fn no_catalog_entry_uses_browser_only_nip07() {
        assert!(known_local_signers().iter().all(|app| {
            app.protocols.iter().all(|protocol| {
                matches!(
                    protocol,
                    LocalSignerProtocol::Nip46 | LocalSignerProtocol::Nip55
                )
            })
        }));
    }
}
