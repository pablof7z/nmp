import NMPFFI

extension NMPEngine {
    /// Protocol-neutral attachment mailbox for separately packaged signer
    /// providers. Provider packages use SPI so ordinary app code continues
    /// to see one `NMPEngine`, not a second engine or provider registry.
    @_spi(NMPProviderComponents)
    public func signerProviderMailbox() -> FfiSignerMailbox {
        ffi.signerMailbox()
    }
}
