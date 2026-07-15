import XCTest
@testable import NMP

final class NIP51Tests: XCTestCase {
    func testAddRelayIsAnNMPActionWithTypedSignedOutFailure() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }

        let statuses = await firstStatuses(
            from: engine.addSimpleGroupRelay("wss://relay.example"),
            count: 2
        )
        XCTAssertEqual(statuses, [.acquiring, .failed(.signedOut)])
    }

    func testInvalidRelayIsTypedActionStateNotANativeException() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }

        let statuses = await firstStatuses(
            from: engine.removeSimpleGroupRelay("not-a-relay"),
            count: 1
        )
        XCTAssertEqual(statuses, [.failed(.invalidRelay("not-a-relay"))])
    }

    private func firstStatuses(
        from action: NMPRelayListAction,
        count: Int
    ) async -> [NMPRelayListActionStatus] {
        await withTaskGroup(of: [NMPRelayListActionStatus].self) { group in
            group.addTask {
                var result: [NMPRelayListActionStatus] = []
                for await status in action.status {
                    result.append(status)
                    if result.count == count { break }
                }
                return result
            }
            group.addTask {
                try? await Task.sleep(nanoseconds: 3_000_000_000)
                return []
            }
            let result = await group.next() ?? []
            group.cancelAll()
            return result
        }
    }
}
