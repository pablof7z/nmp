// "The acceptance test rendered on screen, permanently" (VISION §5; M5 plan
// §3 table row 5, §6 checklist screenshot 5). The ONLY NMP call is
// `engine.observeDiagnostics()`; every field rendered is a REAL number read
// off the running engine (per-relay wire-sub count, exact wire filters,
// events actually received per kind, per-filter coverage, reverse
// coverage/authors-served) -- never fabricated or estimated client-side.

import SwiftUI
import NMP

struct DiagnosticsView: View {
    let model: AppModel

    @State private var snapshot = DiagnosticsSnapshot()

    var body: some View {
        NavigationStack {
            List {
                Section("Summary") {
                    LabeledContent("Relays", value: "\(snapshot.relays.count)")
                    LabeledContent("Uncovered authors", value: "\(snapshot.uncoveredAuthorCount)")
                    if !snapshot.droppedMergeRules.isEmpty {
                        LabeledContent("Dropped merge rules", value: "\(snapshot.droppedMergeRules.count)")
                    }
                }

                ForEach(snapshot.relays) { relay in
                    Section(relay.relay) {
                        LabeledContent("Wire sub count", value: "\(relay.wireSubCount)")
                        LabeledContent("Authors served", value: "\(relay.authorsServed)")

                        if !relay.byLane.isEmpty {
                            ForEach(relay.byLane, id: \.lane) { lane in
                                LabeledContent("lane:\(lane.lane)", value: "\(lane.count)")
                            }
                        }

                        if !relay.filters.isEmpty {
                            DisclosureGroup("Exact wire filters (\(relay.filters.count))") {
                                ForEach(relay.filters, id: \.self) { filterJson in
                                    Text(filterJson)
                                        .font(.caption2.monospaced())
                                        .lineLimit(4)
                                }
                            }
                        }

                        if !relay.eventsByKind.isEmpty {
                            ForEach(relay.eventsByKind, id: \.kind) { kc in
                                LabeledContent("events kind:\(kc.kind)", value: "\(kc.count)")
                            }
                        }

                        if !relay.coverage.isEmpty {
                            ForEach(relay.coverage, id: \.filter) { fc in
                                LabeledContent(coverageLabel(fc.coverage)) {
                                    Text(fc.filter).font(.caption2.monospaced()).lineLimit(2)
                                }
                            }
                        }
                    }
                }
            }
            .navigationTitle("Diagnostics")
            .overlay {
                if snapshot.relays.isEmpty {
                    ContentUnavailableView(
                        "No diagnostics yet",
                        systemImage: "waveform.path.ecg",
                        description: Text("Waiting on the engine to plan a relay subscription.")
                    )
                }
            }
            .task {
                await observe()
            }
        }
    }

    private func coverageLabel(_ coverage: CoverageInterval?) -> String {
        guard let coverage else { return "coverage: unproven" }
        return "coverage: through \(coverage.through)"
    }

    private func observe() async {
        do {
            let diagnostics = try model.engine.observeDiagnostics()
            for await s in diagnostics {
                snapshot = s
            }
        } catch {
            model.lastError = "\(error)"
        }
    }
}
