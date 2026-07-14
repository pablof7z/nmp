import SwiftUI

/// App-owned interaction wrapper installed by nmp-ui.
public struct NMPSourceActionSurface<Content: View>: View {
    private let action: (() -> Void)?
    private let content: Content

    public init(action: (() -> Void)? = nil, @ViewBuilder content: () -> Content) {
        self.action = action
        self.content = content()
    }

    @ViewBuilder
    public var body: some View {
        if let action {
            Button(action: action) { content }
                .buttonStyle(.plain)
        } else {
            content
        }
    }
}
