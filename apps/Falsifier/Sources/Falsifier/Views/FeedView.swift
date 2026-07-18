// $myFollows at depth-1 through the real reducer (M5 plan §3 table row 2,
// the flagship screen). The ONLY NMP call this view makes is
// `engine.observe(FeedFilters.follows(kinds:))` -- everything else
// (formatting pubkeys/timestamps, list rendering) is presentation, which is
// this app's job, never the engine's (`Row` carries raw tokens only).

import SwiftUI
import NMP

struct FeedView: View {
    let model: AppModel

    @State private var rows: [Row] = []
    @State private var evidence: AcquisitionEvidence?

    var body: some View {
        NavigationStack {
            List {
                Section {
                    LabeledContent("Evidence", value: evidenceText)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                ForEach(rows) { row in
                    VStack(alignment: .leading, spacing: 4) {
                        Text(shortHex(row.pubkey))
                            .font(.caption.monospaced())
                            .foregroundStyle(.secondary)
                        Text(row.content)
                            .font(.body)
                        Text(formatted(row.createdAt))
                            .font(.caption2)
                            .foregroundStyle(.tertiary)
                    }
                    .padding(.vertical, 2)
                }
            }
            .navigationTitle("$myFollows")
            .overlay {
                if rows.isEmpty {
                    ContentUnavailableView(
                        "No rows yet",
                        systemImage: "hourglass",
                        description: Text(
                            "Waiting on relays — pick an active account on the Accounts tab."
                        )
                    )
                }
            }
            // Re-observing on `model.kinds` change proves the SDK's filter
            // is a plain value: editing kinds builds a NEW `NMPFilter` and
            // `.task(id:)` tears down the old query / opens a fresh one --
            // no NMP-side "edit a running query" verb exists or is needed.
            .task(id: model.kinds) {
                await observe()
            }
        }
    }

    // Per-source facts only -- never a rolled-up completeness verdict
    // (`docs/design/scoped-evidence-49-12-plan.md` §4). This falsifier's own
    // rendering choice, not an NMP-provided aggregate.
    private var evidenceText: String {
        guard let evidence else {
            return "no evidence yet"
        }
        let sourceCount = evidence.sources.count
        let shortfallCount = evidence.shortfall.count
        return "\(sourceCount) source(s), \(shortfallCount) shortfall fact(s)"
    }

    private func observe() async {
        rows = []
        evidence = nil
        guard let query = try? model.engine.observe(FeedFilters.follows(kinds: model.kinds)) else {
            return
        }
        // #680: an observation is a throwing AsyncSequence; a throw here is
        // terminal teardown, so end the loop quietly.
        do {
            for try await batch in query {
                rows = batch.rows.sorted { $0.createdAt > $1.createdAt }
                evidence = batch.evidence
            }
        } catch {}
    }

    private func shortHex(_ hex: String) -> String {
        guard hex.count > 16 else { return hex }
        return "\(hex.prefix(8))…\(hex.suffix(8))"
    }

    private func formatted(_ unixSeconds: UInt64) -> String {
        Date(timeIntervalSince1970: TimeInterval(unixSeconds))
            .formatted(date: .abbreviated, time: .shortened)
    }
}
