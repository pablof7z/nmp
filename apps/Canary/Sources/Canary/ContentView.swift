import SwiftUI

struct ContentView: View {
    let model: AppModel

    var body: some View {
        TabView {
            AccountsView(model: model)
                .tabItem { Label("Accounts", systemImage: "person.crop.circle") }

            FeedView(model: model)
                .tabItem { Label("Feed", systemImage: "list.bullet.rectangle") }

            NavigationStack { KindsEditorView(model: model) }
                .tabItem { Label("Kinds", systemImage: "slider.horizontal.3") }

            RelaysView(model: model)
                .tabItem { Label("Relays", systemImage: "network") }

            DiagnosticsView(model: model)
                .tabItem { Label("Diagnostics", systemImage: "waveform.path.ecg") }
        }
    }
}
