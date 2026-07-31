import Foundation
import NMPFFI

/// Exact identity embedded by the loaded core native component.
///
/// Provider packages compare this plain value with their own required core
/// identity before requesting an external Rust object from `NMPEngine`.
@_spi(NMPProviderComponents)
public func nmpProviderCoreComponentIdentity() -> String {
    nmpCoreComponentIdentity()
}

/// Identity of the core package's generated component-interface bindings.
@_spi(NMPProviderComponents)
public func nmpProviderComponentInterfaceIdentity() -> String {
    nmpComponentInterfaceIdentity()
}

extension NMPEngine {
    /// Consume one take-once signer contribution through the core-owned
    /// engine door. Provider packages use SPI so ordinary app code continues
    /// to see one `NMPEngine`, not a second engine or provider registry.
    @_spi(NMPProviderComponents)
    public func installSignerProviderAdapter(
        _ adapter: FfiSignerAdapter
    ) throws -> NMPProviderSignerInstallation {
        do {
            return NMPProviderSignerInstallation(
                ffi: try ffi.installSignerAdapter(adapter: adapter)
            )
        } catch let error as FfiSignerAdapterInstallError {
            switch error {
            case .EngineClosed:
                throw NMPProviderSignerInstallError.engineClosed
            case .AdapterAlreadyTaken:
                throw NMPProviderSignerInstallError.adapterAlreadyTaken
            }
        }
    }
}

/// Typed refusal from the core-owned provider installation door.
@_spi(NMPProviderComponents)
public enum NMPProviderSignerInstallError: Error, Sendable, Equatable {
    case engineClosed
    case adapterAlreadyTaken
}

/// Exact provider installation lease. The optional is the lifecycle: close
/// consumes it once, and deinit repeats the same inert take.
@_spi(NMPProviderComponents)
public final class NMPProviderSignerInstallation: @unchecked Sendable {
    private let lock = NSLock()
    private var ffi: FfiSignerAdapterInstallation?

    fileprivate init(ffi: FfiSignerAdapterInstallation) {
        self.ffi = ffi
    }

    @discardableResult
    public func close() -> Bool {
        lock.lock()
        let current = ffi
        ffi = nil
        lock.unlock()
        return current?.uninstall() ?? false
    }

    deinit {
        close()
    }
}
