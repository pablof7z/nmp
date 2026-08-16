// ResumeOnce.swift
//
// Thread-safe "resume exactly once" guard for bridging callback-based APIs
// (Network.framework state handlers, URLSessionWebSocketTask races) into a
// single continuation resume. Swift 6's strict concurrency checking
// correctly rejects a plain captured `var` for this -- these callbacks
// really can run on arbitrary queues/threads.

import Foundation

final class ResumeOnce: @unchecked Sendable {
    private let lock = NSLock()
    private var used = false

    /// Returns `true` exactly once across however many times this is
    /// called from however many threads; `false` every time after.
    func claim() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        if used { return false }
        used = true
        return true
    }
}
