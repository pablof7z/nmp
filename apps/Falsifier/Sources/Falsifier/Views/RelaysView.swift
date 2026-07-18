// Relays derived from follows' kind:10002, ranked app-side by frequency
// (M5 plan §3 table row 4). Proves a depth-1 derived read of a DIFFERENT
// kind, and makes outbox discovery legible: this screen can only ever
// DISPLAY relays -- there is no `relays:` parameter on `NMPFilter` for it to
// feed back into routing (ledger #3). That missing knob is itself the
// thesis evidence, not a limitation of this screen.

import SwiftUI
import NMP

struct RelaysView: View {
    let model: AppModel

    @State private var counts: [String: Int] = [:]

    var body: some View {
        NavigationStack {
            List(ranked, id: \.relay) { entry in
                HStack {
                    Text(entry.relay)
                        .font(.callout.monospaced())
                    Spacer()
                    Text("\(entry.count)")
                        .foregroundStyle(.secondary)
                }
            }
            .navigationTitle("Follows' Relays")
            .overlay {
                if counts.isEmpty {
                    ContentUnavailableView(
                        "No relay lists yet",
                        systemImage: "network.slash",
                        description: Text("Waiting on follows' kind:10002 events.")
                    )
                }
            }
            .task {
                await observe()
            }
        }
    }

    private var ranked: [(relay: String, count: Int)] {
        counts.map { (relay: $0.key, count: $0.value) }.sorted { $0.count > $1.count }
    }

    private func observe() async {
        guard let query = try? model.engine.observe(FeedFilters.followsRelayLists()) else { return }
        // #680: an observation is a throwing AsyncSequence; a throw here is
        // terminal teardown, so end the loop quietly.
        do {
            for try await batch in query {
                var tally: [String: Int] = [:]
                for row in batch.rows {
                    for tag in row.tags where tag.count > 1 && tag[0] == "r" {
                        tally[tag[1], default: 0] += 1
                    }
                }
                counts = tally
            }
        } catch {}
    }
}
