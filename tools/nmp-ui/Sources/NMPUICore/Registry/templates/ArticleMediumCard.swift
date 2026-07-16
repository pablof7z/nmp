import NMPContent
import NMPUI
import SwiftUI

/// App-owned article row installed by nmp-ui. Edit it to match the host app.
public struct NMPSourceArticleMediumCard: View {
    public let article: NMPArticlePresentation
    public let authorProfile: NMPProfilePresentation?
    public let action: (() -> Void)?

    public init(
        article: NMPArticlePresentation,
        authorProfile: NMPProfilePresentation? = nil,
        action: (() -> Void)? = nil
    ) {
        self.article = article
        self.authorProfile = authorProfile
        self.action = action
    }

    public var body: some View {
        NMPSourceActionSurface(action: action) {
            HStack(alignment: .top, spacing: 14) {
                VStack(alignment: .leading, spacing: 7) {
                    HStack(spacing: 6) {
                        NMPAvatar(pubkey: article.author, profile: authorProfile, size: 20)
                        NMPName(pubkey: article.author, profile: authorProfile)
                            .font(.caption.weight(.medium))
                    }

                    NMPArticleTitle(article: article)
                        .font(.headline.weight(.bold))
                        .lineLimit(3)

                    NMPArticleSummary(article: article)
                        .font(.subheadline)
                        .lineLimit(2)

                    NMPArticleReadingTime(article: article)
                        .font(.caption2)
                }
                .frame(maxWidth: .infinity, alignment: .leading)

                NMPArticleImage(article: article, placeholderSystemImage: "doc.richtext")
                    .frame(width: 108, height: 108)
                    .clipShape(RoundedRectangle(cornerRadius: 10))
            }
            .padding(.vertical, 13)
            .contentShape(Rectangle())
        }
        .accessibilityElement(children: .contain)
        .accessibilityHint(action == nil ? "" : "Opens article")
    }
}
