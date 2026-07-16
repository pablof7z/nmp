import NMPContent
import SwiftUI

struct NMPActionSurface<Content: View>: View {
    let action: (() -> Void)?
    let content: Content

    init(action: (() -> Void)?, @ViewBuilder content: () -> Content) {
        self.action = action
        self.content = content()
    }

    @ViewBuilder
    var body: some View {
        if let action {
            Button(action: action) { content }
                .buttonStyle(.plain)
        } else {
            content
        }
    }
}

struct NMPCardInteraction: ViewModifier {
    let accessibilityName: String
    let action: (() -> Void)?

    @ViewBuilder
    func body(content: Content) -> some View {
        if let action {
            content
                .contentShape(Rectangle())
                .onTapGesture(perform: action)
                .accessibilityAction(named: Text(accessibilityName), action)
        } else {
            content
        }
    }
}

public struct NMPAvatarItem: Identifiable, Hashable, Sendable {
    public let pubkey: String
    public let profile: NMPProfilePresentation?

    public var id: String { pubkey }

    public init(pubkey: String, profile: NMPProfilePresentation? = nil) {
        self.pubkey = pubkey
        self.profile = profile
    }
}

/// Reusable overlapping-avatar primitive. It owns only layout; the supplied
/// people and any action remain controlled by the host.
public struct NMPAvatarGroup: View {
    @Environment(\.nmpUITheme) private var theme

    public let people: [NMPAvatarItem]
    public let maximumVisible: Int
    public let size: CGFloat

    public init(
        people: [NMPAvatarItem],
        maximumVisible: Int = 4,
        size: CGFloat = 28
    ) {
        self.people = people
        self.maximumVisible = max(1, maximumVisible)
        self.size = size
    }

    public var overflowCount: Int {
        max(0, people.count - maximumVisible)
    }

    public var body: some View {
        HStack(spacing: -max(4, size * 0.28)) {
            ForEach(Array(people.prefix(maximumVisible).enumerated()), id: \.element.id) { index, person in
                NMPAvatar(pubkey: person.pubkey, profile: person.profile, size: size)
                    .overlay(Circle().stroke(theme.surface, lineWidth: 2))
                    .zIndex(Double(maximumVisible - index))
            }
            if overflowCount > 0 {
                Text("+\(overflowCount)")
                    .font(.system(size: max(9, size * 0.34), weight: .bold, design: .rounded))
                    .foregroundStyle(theme.secondary)
                    .frame(width: size, height: size)
                    .background(theme.elevatedSurface, in: Circle())
                    .overlay(Circle().stroke(theme.surface, lineWidth: 2))
                    .accessibilityLabel("\(overflowCount) more people")
            }
        }
        .accessibilityElement(children: .combine)
    }
}

/// NIP-05 leaf primitive shared by identities and cards.
public struct NMPNIP05: View {
    @Environment(\.nmpUITheme) private var theme
    public let value: String

    public init(_ value: String) {
        self.value = value
    }

    public var body: some View {
        Label(value, systemImage: "checkmark.seal.fill")
            .font(.caption)
            .foregroundStyle(theme.secondary)
            .lineLimit(1)
            .accessibilityLabel("Verified Nostr identifier \(value)")
    }
}

public enum NMPArticleText {
    public static func title(_ article: NMPArticlePresentation) -> String {
        article.title?.trimmingCharacters(in: .whitespacesAndNewlines).nilIfEmpty
            ?? "Untitled article"
    }

    public static func summary(_ article: NMPArticlePresentation) -> String? {
        article.summary?.trimmingCharacters(in: .whitespacesAndNewlines).nilIfEmpty
    }
}

public struct NMPArticleTitle: View {
    @Environment(\.nmpUITheme) private var theme
    public let article: NMPArticlePresentation

    public init(article: NMPArticlePresentation) {
        self.article = article
    }

    public var body: some View {
        Text(NMPArticleText.title(article))
            .foregroundStyle(theme.foreground)
            .multilineTextAlignment(.leading)
    }
}

public struct NMPArticleSummary: View {
    @Environment(\.nmpUITheme) private var theme
    public let article: NMPArticlePresentation

    public init(article: NMPArticlePresentation) {
        self.article = article
    }

    @ViewBuilder
    public var body: some View {
        if let summary = NMPArticleText.summary(article) {
            Text(summary)
                .foregroundStyle(theme.secondary)
                .multilineTextAlignment(.leading)
        }
    }
}

public struct NMPArticleImage: View {
    @Environment(\.nmpUITheme) private var theme
    public let article: NMPArticlePresentation
    public let placeholderSystemImage: String

    public init(
        article: NMPArticlePresentation,
        placeholderSystemImage: String = "doc.text.image"
    ) {
        self.article = article
        self.placeholderSystemImage = placeholderSystemImage
    }

    public var body: some View {
        Group {
            if let image = article.image, let url = URL(string: image) {
                NMPRemoteImage(url: url)
            } else {
                ZStack {
                    NMPAvatar.placeholderColor(for: article.author).opacity(0.18)
                    Image(systemName: placeholderSystemImage)
                        .font(.system(size: 38, weight: .light))
                        .foregroundStyle(theme.secondary)
                }
            }
        }
        .accessibilityLabel(NMPArticleText.title(article))
    }
}

public struct NMPArticleReadingTime: View {
    @Environment(\.nmpUITheme) private var theme
    public let article: NMPArticlePresentation
    public let wordsPerMinute: Int

    public init(article: NMPArticlePresentation, wordsPerMinute: Int = 220) {
        self.article = article
        self.wordsPerMinute = wordsPerMinute
    }

    public var body: some View {
        Text("\(NMPReadingTime.minutes(for: article.content, wordsPerMinute: wordsPerMinute)) min read")
            .foregroundStyle(theme.secondary)
    }
}

public struct NMPArticleByline: View {
    @Environment(\.nmpUITheme) private var theme
    public let article: NMPArticlePresentation
    public let authorProfile: NMPProfilePresentation?
    public let includesDate: Bool

    public init(
        article: NMPArticlePresentation,
        authorProfile: NMPProfilePresentation? = nil,
        includesDate: Bool = true
    ) {
        self.article = article
        self.authorProfile = authorProfile
        self.includesDate = includesDate
    }

    public var body: some View {
        HStack(spacing: 8) {
            NMPAvatar(pubkey: article.author, profile: authorProfile, size: 28)
            VStack(alignment: .leading, spacing: 1) {
                NMPName(pubkey: article.author, profile: authorProfile)
                    .font(.caption.weight(.semibold))
                HStack(spacing: 4) {
                    NMPArticleReadingTime(article: article)
                    if includesDate {
                        Text("·")
                        Text(publishedDate)
                    }
                }
                .font(.caption2)
                .foregroundStyle(theme.secondary)
            }
        }
    }

    private var publishedDate: String {
        let timestamp = article.publishedAt ?? article.createdAt
        return Date(timeIntervalSince1970: TimeInterval(timestamp))
            .formatted(date: .abbreviated, time: .omitted)
    }
}

private extension String {
    var nilIfEmpty: String? { isEmpty ? nil : self }
}
