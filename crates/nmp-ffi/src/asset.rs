//! Protocol-neutral exact-byte asset identity at the native boundary.

/// Return the canonical lowercase SHA-256 identity of the exact supplied
/// bytes. This function performs no network I/O and trusts no claimed digest.
#[uniffi::export]
pub fn asset_sha256_hex(bytes: Vec<u8>) -> String {
    nmp::asset::Sha256Hash::of(&bytes).to_hex()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_bytes_produce_the_known_canonical_digest() {
        assert_eq!(
            asset_sha256_hex(Vec::new()),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
