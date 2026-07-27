import NMPFFI

/// Exact identity embedded by the loaded core native component.
///
/// Provider packages compare this plain value with their own required core
/// identity before requesting an external Rust object from `NMPEngine`.
@_spi(NMPProviderComponents)
public func nmpProviderCoreComponentIdentity() -> String {
    nmpCoreComponentIdentity()
}

extension NMPEngine {
    /// Protocol-neutral attachment mailbox for separately packaged signer
    /// providers. Provider packages use SPI so ordinary app code continues
    /// to see one `NMPEngine`, not a second engine or provider registry.
    @_spi(NMPProviderComponents)
    public func signerProviderMailbox() -> FfiSignerMailbox {
        ffi.signerMailbox()
    }
}
