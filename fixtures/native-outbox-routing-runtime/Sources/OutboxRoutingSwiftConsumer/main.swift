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
                outboxRouting: OutboxRoutingConfig(indexers: [manifest["indexer"]!]),
                allowedLocalRelayHosts: ["localhost", "127.0.0.1"]
            )
        )
        defer { engine.shutdown() }
        let account = try await engine.addAccount(secretKey: manifest["secret_key"]!)
        try engine.setActiveAccount(account.publicKey)
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
