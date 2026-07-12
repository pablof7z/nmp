import NMPContent
import SwiftUI

public enum NMPReadingTime {
    public static func minutes(for content: String, wordsPerMinute: Int = 220) -> Int {
        let words = content.split(whereSeparator: { $0.isWhitespace || $0.isNewline }).count
        return max(1, Int(ceil(Double(words) / Double(max(1, wordsPerMinute)))))
    }
}

/// Large editorial treatment: edge-to-edge lead image, overlaid category cue,
/// then a spacious title/summary/byline stack.
public struct NMPArticlePortraitCard: View {
    @Environment(\.nmpUITheme) private var theme

    public let article: NostrArticle
    public let authorProfile: NostrProfileMetadata?
    public let action: (() -> Void)?

    public init(
        article: NostrArticle,
        authorProfile: NostrProfileMetadata? = nil,
        action: (() -> Void)? = nil
    ) {
        self.article = article
        self.authorProfile = authorProfile
        self.action = action
    }

    public var body: some View {
        Button(action: { action?() }) {
            VStack(alignment: .leading, spacing: 0) {
                leadImage
                    .frame(maxWidth: .infinity)
                    .frame(height: 210)
                    .clipped()
                    .overlay(alignment: .topLeading) {
                        Text("LONG-FORM")
                            .font(.caption2.weight(.bold))
                            .tracking(0.8)
                            .foregroundStyle(.white)
                            .padding(.horizontal, 9)
                            .padding(.vertical, 6)
                            .background(.black.opacity(0.62), in: Capsule())
                            .padding(14)
                    }

                VStack(alignment: .leading, spacing: 12) {
                    Text(title)
                        .font(.system(.title2, design: .serif, weight: .bold))
                        .foregroundStyle(theme.foreground)
                        .multilineTextAlignment(.leading)
                        .lineLimit(3)

                    if let summary = article.summary, !summary.isEmpty {
                        Text(summary)
                            .font(.subheadline)
                            .foregroundStyle(theme.secondary)
                            .multilineTextAlignment(.leading)
                            .lineLimit(3)
                    }

                    Divider().overlay(theme.border)
                    byline
                }
                .padding(18)
            }
            .background(theme.surface, in: RoundedRectangle(cornerRadius: theme.cornerRadius))
            .overlay(RoundedRectangle(cornerRadius: theme.cornerRadius).strokeBorder(theme.border, lineWidth: 0.5))
            .clipShape(RoundedRectangle(cornerRadius: theme.cornerRadius))
        }
        .buttonStyle(.plain)
        .disabled(action == nil)
    }

    @ViewBuilder
    private var leadImage: some View {
        if let image = article.image, let url = URL(string: image) {
            NMPRemoteImage(url: url)
        } else {
            ZStack {
                NMPAvatar.placeholderColor(for: article.author).opacity(0.20)
                Image(systemName: "doc.richtext")
                    .font(.system(size: 46, weight: .light))
                    .foregroundStyle(NMPAvatar.placeholderColor(for: article.author).opacity(0.62))
            }
        }
    }

    private var byline: some View {
        HStack(spacing: 9) {
            NMPAvatar(pubkey: article.author, profile: authorProfile, size: 30)
            VStack(alignment: .leading, spacing: 1) {
                NMPName(pubkey: article.author, profile: authorProfile)
                    .font(.caption.weight(.semibold))
                Text(metadata)
                    .font(.caption2)
                    .foregroundStyle(theme.secondary)
            }
            Spacer()
            Image(systemName: "arrow.up.right")
                .font(.caption.weight(.semibold))
                .foregroundStyle(theme.secondary)
        }
    }

    private var title: String {
        article.title?.trimmingCharacters(in: .whitespacesAndNewlines).nilIfEmpty
            ?? "Untitled article"
    }

    private var metadata: String {
        "\(NMPReadingTime.minutes(for: article.content)) min read · \(publishedDate)"
    }

    private var publishedDate: String {
        let timestamp = article.publishedAt ?? article.createdAt
        return Date(timeIntervalSince1970: TimeInterval(timestamp)).formatted(date: .abbreviated, time: .omitted)
    }
}

/// Medium-like list treatment: dense text hierarchy on the left and a fixed
/// editorial thumbnail on the right. This is intentionally a different
/// composition from the portrait card, not a resized version of it.
public struct NMPArticleMediumCard: View {
    @Environment(\.nmpUITheme) private var theme

    public let article: NostrArticle
    public let authorProfile: NostrProfileMetadata?
    public let action: (() -> Void)?

    public init(
        article: NostrArticle,
        authorProfile: NostrProfileMetadata? = nil,
        action: (() -> Void)? = nil
    ) {
        self.article = article
        self.authorProfile = authorProfile
        self.action = action
    }

    public var body: some View {
        Button(action: { action?() }) {
            HStack(alignment: .top, spacing: 14) {
                VStack(alignment: .leading, spacing: 7) {
                    HStack(spacing: 6) {
                        NMPAvatar(pubkey: article.author, profile: authorProfile, size: 20)
                        NMPName(pubkey: article.author, profile: authorProfile)
                            .font(.caption.weight(.medium))
                            .foregroundStyle(theme.foreground)
                    }

                    Text(title)
                        .font(.headline.weight(.bold))
                        .foregroundStyle(theme.foreground)
                        .multilineTextAlignment(.leading)
                        .lineLimit(3)

                    if let summary = article.summary, !summary.isEmpty {
                        Text(summary)
                            .font(.subheadline)
                            .foregroundStyle(theme.secondary)
                            .multilineTextAlignment(.leading)
                            .lineLimit(2)
                    }

                    Text("\(NMPReadingTime.minutes(for: article.content)) min read")
                        .font(.caption2)
                        .foregroundStyle(theme.secondary)
                }
                .frame(maxWidth: .infinity, alignment: .leading)

                thumbnail
                    .frame(width: 108, height: 108)
                    .clipShape(RoundedRectangle(cornerRadius: 10))
            }
            .padding(.vertical, 13)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(action == nil)
    }

    @ViewBuilder
    private var thumbnail: some View {
        if let image = article.image, let url = URL(string: image) {
            NMPRemoteImage(url: url)
        } else {
            ZStack {
                theme.elevatedSurface
                Image(systemName: "doc.text.image")
                    .font(.title2)
                    .foregroundStyle(theme.secondary)
            }
        }
    }

    private var title: String {
        article.title?.trimmingCharacters(in: .whitespacesAndNewlines).nilIfEmpty
            ?? "Untitled article"
    }
}

private extension String {
    var nilIfEmpty: String? { isEmpty ? nil : self }
}
