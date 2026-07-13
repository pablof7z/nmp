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
        NMPActionSurface(action: action) {
            card
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel(NMPArticleText.title(article))
        .accessibilityHint(action == nil ? "" : "Opens article")
    }

    private var card: some View {
        VStack(alignment: .leading, spacing: 0) {
            NMPArticleImage(article: article, placeholderSystemImage: "doc.richtext")
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
                NMPArticleTitle(article: article)
                    .font(.system(.title2, design: .serif, weight: .bold))
                    .lineLimit(3)

                if NMPArticleText.summary(article) != nil {
                    NMPArticleSummary(article: article)
                        .font(.subheadline)
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

    private var byline: some View {
        HStack(spacing: 9) {
            NMPArticleByline(article: article, authorProfile: authorProfile)
            Spacer()
            Image(systemName: "arrow.up.right")
                .font(.caption.weight(.semibold))
                .foregroundStyle(theme.secondary)
                .accessibilityHidden(true)
        }
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
        NMPActionSurface(action: action) {
            HStack(alignment: .top, spacing: 14) {
                VStack(alignment: .leading, spacing: 7) {
                    HStack(spacing: 6) {
                        NMPAvatar(pubkey: article.author, profile: authorProfile, size: 20)
                        NMPName(pubkey: article.author, profile: authorProfile)
                            .font(.caption.weight(.medium))
                            .foregroundStyle(theme.foreground)
                    }

                    NMPArticleTitle(article: article)
                        .font(.headline.weight(.bold))
                        .lineLimit(3)

                    if NMPArticleText.summary(article) != nil {
                        NMPArticleSummary(article: article)
                            .font(.subheadline)
                            .lineLimit(2)
                    }

                    NMPArticleReadingTime(article: article)
                        .font(.caption2)
                }
                .frame(maxWidth: .infinity, alignment: .leading)

                thumbnail
                    .frame(width: 108, height: 108)
                    .clipShape(RoundedRectangle(cornerRadius: 10))
            }
            .padding(.vertical, 13)
            .contentShape(Rectangle())
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel(NMPArticleText.title(article))
        .accessibilityHint(action == nil ? "" : "Opens article")
    }

    private var thumbnail: some View {
        NMPArticleImage(article: article)
    }
}
