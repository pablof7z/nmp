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

private final class SignEventBridge: SignEventObserver, @unchecked Sendable {
    private let lock = NSLock()
    private var continuation: CheckedContinuation<NMPSignedEvent, Error>?
    private var handle: NmpSignEventHandle?
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

    func install(_ handle: NmpSignEventHandle) {
        lock.lock()
        let alreadyCompleted = completed
        if !alreadyCompleted {
            self.handle = handle
        }
        let cancelled = cancellationRequested
        lock.unlock()
        if !alreadyCompleted && cancelled {
            handle.cancel()
        }
    }

    func failToStart(_ error: Error) {
        finish(.failure(error))
    }

    func requestCancellation() {
        lock.lock()
        cancellationRequested = true
        let handle = handle
        lock.unlock()
        handle?.cancel()
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
        self.handle = nil
        lock.unlock()
        continuation.resume(with: result)
    }
}

extension NMPEngine {
    /// Sign one exact event with the active signer without creating a write
    /// intent, pending row, receipt, relay plan, or publication.
    public func signEvent(_ event: NMPUnsignedEvent) async throws -> NMPSignedEvent {
        let bridge = SignEventBridge()
        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                bridge.start(continuation)
                do {
                    let handle = try nmpRethrowing {
                        try ffi.signEvent(event: event.toFfi(), observer: bridge)
                    }
                    bridge.install(handle)
                } catch {
                    bridge.failToStart(error)
                }
            }
        } onCancel: {
            bridge.requestCancellation()
        }
    }
}
