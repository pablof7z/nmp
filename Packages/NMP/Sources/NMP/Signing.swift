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
    private let lock = NSLock()
    private var continuation: CheckedContinuation<NMPSignedEvent, Error>?
    private var cancelOperation: (() -> Void)?
    private var cancellationRequested = false
    private var completed = false

    func start(_ continuation: CheckedContinuation<NMPSignedEvent, Error>) {
        lock.lock()
        self.continuation = continuation
        let cancelled = cancellationRequested
        lock.unlock()
        if cancelled {
            requestCancellation()
        }
    }

    func installCancellation(_ cancelOperation: @escaping () -> Void) {
        lock.lock()
        let alreadyCompleted = completed
        if !alreadyCompleted {
            self.cancelOperation = cancelOperation
        }
        let cancelled = cancellationRequested
        lock.unlock()
        if !alreadyCompleted && cancelled {
            cancelOperation()
        }
    }

    func failToStart(_ error: Error) {
        finish(.failure(error))
    }

    func requestCancellation() {
        lock.lock()
        cancellationRequested = true
        let cancelOperation = cancelOperation
        lock.unlock()
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
        guard !completed, let continuation else {
            lock.unlock()
            return
        }
        completed = true
        self.continuation = nil
        self.cancelOperation = nil
        lock.unlock()
        continuation.resume(with: result)
    }
}

func performSignEvent(
    _ event: NMPUnsignedEvent,
    start: (FfiSignEventRequest, SignEventObserver) throws -> (() -> Void)
) async throws -> NMPSignedEvent {
    let bridge = SignEventBridge()
    return try await withTaskCancellationHandler {
        try await withCheckedThrowingContinuation { continuation in
            bridge.start(continuation)
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
