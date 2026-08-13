import Foundation
import NMP

@main
struct OutboxRoutingSwiftConsumer {
    static func main() async throws {
        let manifest = try JSONSerialization.jsonObject(
            with: Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[1]))
        ) as! [String: String]
        let engine = try NMPEngine(
            config: NMPConfig(
                outboxRouting: OutboxRoutingConfig(indexers: [manifest["indexer"]!])
            )
        )
        defer { engine.shutdown() }
        let privateKey = try NMPPrivateKey(
            bytes: decodedHex(manifest["secret_key"]!)
        )
        _ = try engine.session.add(privateKey: privateKey, makeCurrent: true)
        let receipt = try await engine.publish(
            WriteIntent(
                payload: .event(kind: 1, content: "swift prepared cold discovery"),
                routing: .auto
            )
        )
        let result = try await receipt.result()
        guard result.outcome == .settled,
              result.relays.count == 1,
              result.relays[0].relay == manifest["outbox"],
              result.relays[0].state == .published else {
            fatalError("unexpected Swift receipt: \(result)")
        }
        print("PASS swift prepared outbox routing cold discovery")
    }
}

private enum FixtureInputError: Error {
    case invalidHex
}

private func decodedHex(_ hex: String) throws -> Data {
    guard hex.count.isMultiple(of: 2) else {
        throw FixtureInputError.invalidHex
    }
    var bytes = Data()
    bytes.reserveCapacity(hex.count / 2)
    var index = hex.startIndex
    while index < hex.endIndex {
        let next = hex.index(index, offsetBy: 2)
        guard let byte = UInt8(hex[index..<next], radix: 16) else {
            throw FixtureInputError.invalidHex
        }
        bytes.append(byte)
        index = next
    }
    return bytes
}
