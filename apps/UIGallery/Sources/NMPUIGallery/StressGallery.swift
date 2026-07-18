import NMP
import NMPContent
import NMPUI
import SwiftUI

struct StressGallery: View {
    let model: GalleryModel
    @State private var diagnostics = DiagnosticsSnapshot()

    var body: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 0) {
                GalleryIntro(
                    eyebrow: "RAPID-SCROLL FALSIFIER",
                    title: "Seventy-two rows. Two independently owned references each.",
                    description: "Every row uses the production content view. Each selected component owns and releases its ordinary NMP handle as visibility changes; equal core demand may still coalesce."
                )
                .padding(.bottom, 18)

                metrics
                    .padding(.bottom, 14)

                ForEach(Array(model.stressDocuments.enumerated()), id: \.offset) { index, document in
                    StressContentRow(
                        index: index,
                        document: document,
                        observationFactory: model.observationFactory
                    )
                    Divider()
                }

                Text("End of 72-row stress list")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 24)
                    .accessibilityIdentifier("gallery.stress.end")
            }
            .padding(.horizontal, 20)
        }
        .navigationTitle("Stress")
        .navigationBarTitleDisplayMode(.inline)
        .accessibilityIdentifier("gallery.stress.scroll")
        .task { await observeDiagnostics() }
    }

    private var metrics: some View {
        HStack(spacing: 12) {
            Image(systemName: "waveform.path.ecg")
                .foregroundStyle(.purple)
            VStack(alignment: .leading, spacing: 2) {
                Text("\(wireSubscriptionCount) engine wire subscriptions")
                    .font(.headline)
                    .accessibilityIdentifier("gallery.stress.wire-count")
                Text("This is observed engine evidence, not a UI-owned claim counter or budget.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.purple.opacity(0.08), in: RoundedRectangle(cornerRadius: 16))
    }

    private var wireSubscriptionCount: UInt32 {
        diagnostics.relays.reduce(0) { $0 + $1.wireSubCount }
    }

    private func observeDiagnostics() async {
        guard let stream = try? model.engine.observeDiagnostics() else { return }
        // #680: observations are throwing AsyncSequences; a throw is terminal.
        do {
            for try await snapshot in stream {
                diagnostics = snapshot
            }
        } catch {}
    }
}

private struct StressContentRow: View {
    let index: Int
    let document: NostrContentDocument
    let observationFactory: NMPReferenceObservationFactory

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Text("\(index + 1)")
                .font(.caption2.monospacedDigit())
                .foregroundStyle(.secondary)
                .frame(width: 24, alignment: .trailing)
            NostrContent(
                document: document,
                observationFactory: observationFactory,
                purpose: .preview,
                renderers: stressRenderers,
                maximumBlocks: 1,
                maximumLinesPerBlock: 2
            )
            .font(.subheadline)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.vertical, 11)
        .accessibilityIdentifier("gallery.stress.row.\(index + 1)")
    }

    private var stressRenderers: NostrContentRenderers {
        NostrContentRenderers.standard
            .fallbackEvent(layout: .inline) { input in
                Text(input.event.content)
                    .lineLimit(1)
            }
            .unresolvedReference(layout: .inline) { _ in
                Text("resolving…")
                    .foregroundStyle(.secondary)
            }
    }
}
