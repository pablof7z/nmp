import NMPContent
import NMPUI
import SwiftUI

struct StressGallery: View {
    @ObservedObject var model: GalleryModel

    var body: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 0) {
                GalleryIntro(
                    eyebrow: "RAPID-SCROLL FALSIFIER",
                    title: "Seventy-two rows. Two live references each.",
                    description: "Every row uses the production content view. Claims follow visibility, sessions cap their own work, and ordinary NMP demand coalesces repeated profile/event targets."
                )
                .padding(.bottom, 18)

                metrics
                    .padding(.bottom, 14)

                ForEach(Array(model.stressSessions.enumerated()), id: \.offset) { index, session in
                    StressContentRow(index: index, session: session)
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
    }

    private var metrics: some View {
        HStack(spacing: 12) {
            Image(systemName: "waveform.path.ecg")
                .foregroundStyle(.purple)
            VStack(alignment: .leading, spacing: 2) {
                Text("\(model.stressActiveReferences) visible reference claims")
                    .font(.headline)
                    .accessibilityIdentifier("gallery.stress.active-count")
                Text("The number should rise and fall with visible rows—not grow toward 144.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.purple.opacity(0.08), in: RoundedRectangle(cornerRadius: 16))
    }
}

private struct StressContentRow: View {
    let index: Int
    @ObservedObject var session: NostrContentSession

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Text("\(index + 1)")
                .font(.caption2.monospacedDigit())
                .foregroundStyle(.secondary)
                .frame(width: 24, alignment: .trailing)
            NostrContent(
                session: session,
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
            .profileMention { input in
                NMPProfileMention(
                    pubkey: input.pubkey,
                    profile: input.profile,
                    variant: .avatar
                )
            }
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
