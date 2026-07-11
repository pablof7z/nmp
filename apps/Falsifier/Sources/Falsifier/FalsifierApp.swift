// M5 -- the iOS falsifier app's entry point. Plain SwiftUI `App`; NMP is
// not in charge of this at all -- it only becomes a `let` inside
// `AppModel`, constructed on first appearance.

import SwiftUI

@main
struct FalsifierApp: App {
    @State private var model: AppModel?
    @State private var initError: String?

    var body: some Scene {
        WindowGroup {
            Group {
                if let model {
                    ContentView(model: model)
                } else if let initError {
                    VStack(spacing: 12) {
                        Image(systemName: "exclamationmark.triangle")
                            .font(.largeTitle)
                        Text("Engine init failed")
                            .font(.headline)
                        Text(initError)
                            .font(.caption)
                            .multilineTextAlignment(.center)
                    }
                    .padding()
                } else {
                    ProgressView("Starting NMP engine…")
                        .task { setUp() }
                }
            }
        }
    }

    private func setUp() {
        do {
            model = try AppModel()
        } catch {
            initError = "\(error)"
        }
    }
}
