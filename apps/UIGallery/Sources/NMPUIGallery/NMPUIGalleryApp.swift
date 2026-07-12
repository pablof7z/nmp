import SwiftUI

@main
struct NMPUIGalleryApp: App {
    @State private var model: GalleryModel?
    @State private var startupError: String?

    var body: some Scene {
        WindowGroup {
            Group {
                if let model {
                    GalleryRootView(model: model)
                } else if let startupError {
                    ContentUnavailableView(
                        "NMP could not start",
                        systemImage: "exclamationmark.triangle",
                        description: Text(startupError)
                    )
                } else {
                    ProgressView("Starting the real NMP engine…")
                        .task { start() }
                }
            }
        }
    }

    @MainActor
    private func start() {
        do {
            model = try GalleryModel()
        } catch {
            startupError = String(describing: error)
        }
    }
}
