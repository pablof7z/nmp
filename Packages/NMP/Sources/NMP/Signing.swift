import Foundation
import NMPFFI

/// Immutable event body for `NMPEngine.signEvent`. NMP freezes the author
/// from the engine's active account before invoking the signer.
public struct NMPUnsignedEvent: Sendable, Hashable {
    public let createdAt: UInt64
    public let kind: UInt16
    public let tags: [[String]]
    public let content: String

    public init(
        createdAt: UInt64,
        kind: UInt16,
        tags: [[String]],
        content: String
    ) {
        self.createdAt = createdAt
        self.kind = kind
        self.tags = tags
        self.content = content
    }

    func toFfi() -> FfiSignEventRequest {
        FfiSignEventRequest(
            createdAt: createdAt,
            kind: kind,
            tags: tags,
            content: content
        )
    }
}

/// Exact verified result of a sign-only operation. Receiving this value does
/// not imply storage, publication, or creation of a write receipt.
public struct NMPSignedEvent: Sendable, Hashable {
    public let id: String
    public let pubkey: String
    public let createdAt: UInt64
    public let kind: UInt16
    public let tags: [[String]]
    public let content: String
    public let signature: String

    init(_ ffi: FfiSignedEvent) {
        id = ffi.id
        pubkey = ffi.pubkey
        createdAt = ffi.createdAt
        kind = ffi.kind
        tags = ffi.tags
        content = ffi.content
        signature = ffi.sig
    }
}

final class SignEventBridge: SignEventObserver, @unchecked Sendable {
    private enum TerminalState {
        case open
        case cancelled
        case completed
    }

    private let lock = NSLock()
    private var continuation: CheckedContinuation<NMPSignedEvent, Error>?
    private var cancelOperation: (() -> Void)?
    private var terminalState = TerminalState.open

    @discardableResult
    func installContinuation(
        _ continuation: CheckedContinuation<NMPSignedEvent, Error>
    ) -> Bool {
        lock.lock()
        switch terminalState {
        case .open:
            self.continuation = continuation
            lock.unlock()
            return true
        case .cancelled:
            lock.unlock()
            continuation.resume(throwing: CancellationError())
            return false
        case .completed:
            lock.unlock()
            return false
        }
    }

    func installCancellation(_ cancelOperation: @escaping () -> Void) {
        lock.lock()
        switch terminalState {
        case .open:
            self.cancelOperation = cancelOperation
            lock.unlock()
        case .cancelled:
            lock.unlock()
            cancelOperation()
        case .completed:
            lock.unlock()
        }
    }

    func failToStart(_ error: Error) {
        finish(.failure(error))
    }

    func requestCancellation() {
        lock.lock()
        guard case .open = terminalState else {
            lock.unlock()
            return
        }
        terminalState = .cancelled
        let continuation = continuation
        self.continuation = nil
        let cancelOperation = cancelOperation
        self.cancelOperation = nil
        lock.unlock()
        continuation?.resume(throwing: CancellationError())
        cancelOperation?()
    }

    func onSigned(event: FfiSignedEvent) {
        finish(.success(NMPSignedEvent(event)))
    }

    func onFailed(failure: FfiSignEventFailure) {
        switch failure {
        case .signerUnavailable(let reason):
            finish(.failure(NMPError.signerUnavailable(reason)))
        case .signerRejected(let reason):
            finish(.failure(NMPError.signerRejected(reason)))
        case .invalidSignerOutput(let reason):
            finish(.failure(NMPError.invalidSignerOutput(reason)))
        case .cancelled:
            finish(.failure(CancellationError()))
        }
    }

    private func finish(_ result: Result<NMPSignedEvent, Error>) {
        lock.lock()
        guard terminalState == .open else {
            lock.unlock()
            return
        }
        terminalState = .completed
        let continuation = continuation
        self.continuation = nil
        self.cancelOperation = nil
        lock.unlock()
        continuation?.resume(with: result)
    }
}

func performSignEvent(
    _ event: NMPUnsignedEvent,
    start: (FfiSignEventRequest, SignEventObserver) throws -> (() -> Void)
) async throws -> NMPSignedEvent {
    let bridge = SignEventBridge()
    return try await withTaskCancellationHandler {
        try await withCheckedThrowingContinuation { continuation in
            guard bridge.installContinuation(continuation) else {
                return
            }
            do {
                bridge.installCancellation(try start(event.toFfi(), bridge))
            } catch {
                bridge.failToStart(error)
            }
        }
    } onCancel: {
        bridge.requestCancellation()
    }
}

extension NMPEngine {
    /// Sign one exact event with the active signer without creating a write
    /// intent, pending row, receipt, relay plan, or publication.
    public func signEvent(_ event: NMPUnsignedEvent) async throws -> NMPSignedEvent {
        try await performSignEvent(event) { request, observer in
            let handle = try nmpRethrowing {
                try ffi.signEvent(event: request, observer: observer)
            }
            return { handle.cancel() }
        }
    }
}
