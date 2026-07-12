// The ergonomic error surface: `NMP`'s public API never leaks the
// `Ffi*`-prefixed generated types (M4 plan §2/§9 -- "hide the Ffi* types
// behind ergonomic Swift enums/structs"). `FfiError` already conforms to
// `Swift.Error`, but re-wrapping it keeps the public surface entirely free
// of the `Ffi` prefix, matching every other value type in this package.

import NMPFFI

/// Every way a call into the engine can fail -- typed states, never a crash
/// (mirrors `nmp-ffi`'s own `FfiError`; see that type's doc for the Rust
/// side of each case).
///
/// NOTE: there is deliberately no `.invalidSignedEvent` case anymore -- a
/// `WritePayload.signed` event that fails `nostr::Event::verify` is no
/// longer rejected synchronously here (#52 Unit B: the guarantee moved to
/// `nmp-engine`'s acceptance boundary so it holds for every entry point, not
/// only this one). It surfaces on `Receipt.status` instead, as
/// `WriteStatus.failed`, the first and only status delivered.
///
/// Also no `.signerHasNoPublicKey` case: `addAccount` goes through
/// `nmp::Engine::add_account`, whose built-in `LocalKeySigner` path always
/// reports a public key -- there is no reachable "signer has no public key"
/// state through this entry point, so an impossible error case is not kept
/// on the public surface just in case.
public enum NMPError: Error, Sendable, Equatable {
    case nonIndexableFilterTag(String)
    case invalidPublicKey(String)
    case invalidEventId(String)
    case invalidRelayUrl(String)
    case invalidTag([String])
    case invalidSecretKey
    case receiptCorrelationIdExhausted
    case storeOpenFailed(String)
    case invalidSignature(String)
    case engineClosed

    init(_ ffi: FfiError) {
        switch ffi {
        case .NonIndexableFilterTag(let got): self = .nonIndexableFilterTag(got)
        case .InvalidPublicKey(let got): self = .invalidPublicKey(got)
        case .InvalidEventId(let got): self = .invalidEventId(got)
        case .InvalidRelayUrl(let got): self = .invalidRelayUrl(got)
        case .InvalidTag(let got): self = .invalidTag(got)
        case .InvalidSecretKey: self = .invalidSecretKey
        case .ReceiptCorrelationIdExhausted: self = .receiptCorrelationIdExhausted
        case .StoreOpenFailed(let reason): self = .storeOpenFailed(reason)
        case .InvalidSignature(let got): self = .invalidSignature(got)
        case .EngineClosed: self = .engineClosed
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
