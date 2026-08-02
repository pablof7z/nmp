import Foundation
import Darwin
import NMP

@main
struct NIP29ConsumerMain {
    static func main() async {
        do {
            let args = try Args.parse()
            switch args.mode {
            case .online: try await Probe.online(args)
            case .provenanceGrowth: try await Probe.provenanceGrowth(args)
            case .restart: try await Probe.restart(args)
            }
        } catch {
            FileHandle.standardError.write(Data("FAIL \(error)\n".utf8))
            exit(1)
        }
    }
}
