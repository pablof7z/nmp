// The app-owned NIP-42 authorization surface (#8): opaque registration
// proofs for exact-instance removal, plus the completion-only policy
// callback the engine consults before signing any AUTH proof.

import NMPFFI

/// Opaque proof of one exact local-account signer installation.
///
/// The public key is presentation and activation data. Removal still requires
/// this object, so a stale cleanup cannot remove a same-key replacement.
public final class NMPAccountRegistration: @unchecked Sendable {
    public let publicKey: String
    let ffi: FfiAccountRegistration

    init(ffi: FfiAccountRegistration) {
        self.ffi = ffi
        publicKey = ffi.publicKey()
    }
}

/// Every immutable fact supplied to app-owned NIP-42 authorization policy.
public struct NMPAuthPolicyRequest: Sendable, Hashable {
    public let expectedPublicKey: String
    public let relay: String
    public let challenge: String
    public let transportGeneration: UInt64
    public let epochSequence: UInt64

    public init(
        expectedPublicKey: String,
        relay: String,
        challenge: String,
        transportGeneration: UInt64,
        epochSequence: UInt64
    ) {
        self.expectedPublicKey = expectedPublicKey
        self.relay = relay
        self.challenge = challenge
        self.transportGeneration = transportGeneration
        self.epochSequence = epochSequence
    }

    init(_ ffi: FfiAuthPolicyRequest) {
        self.init(
            expectedPublicKey: ffi.expectedPublicKey,
            relay: ffi.relay,
            challenge: ffi.challenge,
            transportGeneration: ffi.transportGeneration,
            epochSequence: ffi.epochSequence
        )
    }
}

/// The closed result vocabulary for one authorization-policy request.
public enum NMPAuthPolicyOutcome: Sendable, Equatable {
    case allow
    case deny(reason: String)
    case unavailable
    case technical(reason: String)

    var ffi: FfiAuthPolicyOutcome {
        switch self {
        case .allow: .allow
        case .deny(let reason): .deny(reason: reason)
        case .unavailable: .unavailable
        case .technical(let reason): .technical(reason: reason)
        }
    }
}

/// Exact terminal ownership failures from resolving one policy completion.
public enum NMPAuthPolicyCompletionError: Error, Sendable, Equatable {
    case alreadyCompleted
    case cancelled
    case receiverGone

    init(_ ffi: FfiAuthPolicyCompletionError) {
        switch ffi {
        case .AlreadyCompleted: self = .alreadyCompleted
        case .Cancelled: self = .cancelled
        case .ReceiverGone: self = .receiverGone
        }
    }
}

/// One-shot completion for an app-owned authorization decision.
///
/// Resolve inline for an immediate result, retain this object to resolve
/// asynchronously, or return without retaining/resolving to report
/// `.unavailable`. There is no parallel return-value policy path.
public final class NMPAuthPolicyCompletion: @unchecked Sendable {
    private let ffi: FfiAuthPolicyCompletion

    init(_ ffi: FfiAuthPolicyCompletion) {
        self.ffi = ffi
    }

    public func resolve(_ outcome: NMPAuthPolicyOutcome) throws {
        do {
            try ffi.resolve(outcome: outcome.ffi)
        } catch let error as FfiAuthPolicyCompletionError {
            throw NMPAuthPolicyCompletionError(error)
        }
    }
}

/// App-owned NIP-42 authorization policy.
///
/// `evaluate` is intentionally completion-only. `onCancelled` runs only when
/// runtime cancellation wins terminal ownership and may safely reenter NMP.
public protocol NMPAuthPolicy: AnyObject, Sendable {
    func evaluate(
        request: NMPAuthPolicyRequest,
        completion: NMPAuthPolicyCompletion
    )

    func onCancelled(request: NMPAuthPolicyRequest)
}

public extension NMPAuthPolicy {
    func onCancelled(request: NMPAuthPolicyRequest) {}
}

final class AuthPolicyBridge: FfiAuthPolicyCallback, @unchecked Sendable {
    private let policy: any NMPAuthPolicy

    init(policy: any NMPAuthPolicy) {
        self.policy = policy
    }

    func evaluate(request: FfiAuthPolicyRequest, completion: FfiAuthPolicyCompletion) {
        policy.evaluate(
            request: NMPAuthPolicyRequest(request),
            completion: NMPAuthPolicyCompletion(completion)
        )
    }

    func onCancelled(request: FfiAuthPolicyRequest) {
        policy.onCancelled(request: NMPAuthPolicyRequest(request))
    }
}

/// Opaque proof of one exact AUTH-policy installation.
public final class NMPAuthPolicyRegistration: @unchecked Sendable {
    public let publicKey: String
    let ffi: FfiAuthPolicyRegistration

    init(ffi: FfiAuthPolicyRegistration) {
        self.ffi = ffi
        publicKey = ffi.expectedPublicKey()
    }
}

extension NMPEngine {
    /// Install `policy` for one exact expected account identity.
    public func addAuthPolicy(
        expectedPublicKey: String,
        policy: any NMPAuthPolicy
    ) throws -> NMPAuthPolicyRegistration {
        let registration = try nmpRethrowing {
            try ffi.addAuthPolicy(
                expectedPublicKey: expectedPublicKey,
                callback: AuthPolicyBridge(policy: policy)
            )
        }
        return NMPAuthPolicyRegistration(ffi: registration)
    }

    /// Remove only the policy installation proven by `registration`.
    /// Repeated and stale removals return `false`.
    public func removeAuthPolicy(_ registration: NMPAuthPolicyRegistration) throws -> Bool {
        try nmpRethrowing {
            try ffi.removeAuthPolicy(registration: registration.ffi)
        }
    }
}
