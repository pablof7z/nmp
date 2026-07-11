// The ergonomic error surface: `NMP`'s public API never leaks the
// `Ffi*`-prefixed generated types (M4 plan §2/§9 -- "hide the Ffi* types
// behind ergonomic Swift enums/structs"). `FfiError` already conforms to
// `Swift.Error`, but re-wrapping it keeps the public surface entirely free
// of the `Ffi` prefix, matching every other value type in this package.

import NMPFFI

/// Every way a call into the engine can fail -- typed states, never a crash
/// (mirrors `nmp-ffi`'s own `FfiError`; see that type's doc for the Rust
/// side of each case).
public enum NMPError: Error, Sendable, Equatable {
    case invalidTagName(String)
    case invalidPublicKey(String)
    case invalidRelayUrl(String)
    case invalidSecretKey
    case signerHasNoPublicKey
    case storeOpenFailed(String)

    init(_ ffi: FfiError) {
        switch ffi {
        case .InvalidTagName(let got): self = .invalidTagName(got)
        case .InvalidPublicKey(let got): self = .invalidPublicKey(got)
        case .InvalidRelayUrl(let got): self = .invalidRelayUrl(got)
        case .InvalidSecretKey: self = .invalidSecretKey
        case .SignerHasNoPublicKey: self = .signerHasNoPublicKey
        case .StoreOpenFailed(let reason): self = .storeOpenFailed(reason)
        }
    }
}

/// Runs `body`, translating any thrown `FfiError` into the ergonomic
/// `NMPError` -- the one seam every call into `NMPFFI` passes through.
func nmpRethrowing<T>(_ body: () throws -> T) throws -> T {
    do {
        return try body()
    } catch let error as FfiError {
        throw NMPError(error)
    }
}
