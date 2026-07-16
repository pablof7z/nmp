import NMPContent
import NMPUI
import SwiftUI

@main
struct NMPUISampleApp: App {
    private let article = NMPArticlePresentation(
        author: String(repeating: "b", count: 64),
        createdAt: 1_723_456_789,
        title: "Source-owned composition, linked semantics",
        summary: "The card is editable app source; protocol and media behavior stay linked.",
        image: "https://example.invalid/article.jpg",
        content: "Fixture content"
    )

    var body: some Scene {
        WindowGroup {
            NMPSourceArticleMediumCard(article: article, action: openArticle)
                .nmpUITheme(
                    NMPUITheme(
                        accent: .indigo,
                        cornerRadius: 14
                    )
                )
                // Remote media remains an explicit app policy. The fixture
                // keeps it disabled while proving the wiring boundary.
                .nmpImageLoader(.disabled)
                .padding(24)
                .frame(width: 620)
        }
    }

    private func openArticle() {
        // Navigation belongs to the host application.
    }
}
