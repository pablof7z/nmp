// ChildProcess.swift
//
// The bounded "did the OS process actually exit" wait, factored out so a
// scenario driving a plain (non-relay) child process directly -- e.g.
// C9's crash-during-publication harness, which has to `kill -9` its own
// helper executable -- reuses the exact mechanism `RelayHandle` already
// proved, rather than a second hand-rolled copy of it. A real SIGKILL,
// then a bounded poll for `Process.isRunning` going false: never a fixed
// sleep as the oracle, never blocks forever.

import Foundation
#if canImport(Darwin)
import Darwin
#endif

public enum ChildProcess {
    /// SIGKILL, then wait until the process has actually exited or
    /// `timeout` elapses (bounded poll, ~50ms interval -- the same
    /// interval `RelayHandle`'s own stop/kill uses).
    public static func killAndWaitForExit(_ process: Process, timeout: TimeInterval = 5) async {
        guard process.isRunning else { return }
        Foundation.kill(process.processIdentifier, SIGKILL)
        let deadline = Date().addingTimeInterval(timeout)
        while process.isRunning, Date() < deadline {
            try? await Task.sleep(nanoseconds: 50_000_000)
        }
    }
}
