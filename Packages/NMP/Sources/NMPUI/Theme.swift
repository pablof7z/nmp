import SwiftUI

/// Shared visual tokens for the ready-made components. Apps can replace the
/// complete value at any subtree without installing a provider at app root.
public struct NMPUITheme: Sendable {
    public var accent: Color
    public var foreground: Color
    public var secondary: Color
    public var surface: Color
    public var elevatedSurface: Color
    public var border: Color
    public var cornerRadius: CGFloat

    public init(
        accent: Color = Color(red: 0.45, green: 0.22, blue: 0.93),
        foreground: Color = .primary,
        secondary: Color = .secondary,
        surface: Color = Color.primary.opacity(0.045),
        elevatedSurface: Color = Color.primary.opacity(0.075),
        border: Color = Color.primary.opacity(0.10),
        cornerRadius: CGFloat = 18
    ) {
        self.accent = accent
        self.foreground = foreground
        self.secondary = secondary
        self.surface = surface
        self.elevatedSurface = elevatedSurface
        self.border = border
        self.cornerRadius = cornerRadius
    }
}

/// App-owned remote image policy. Remote loading is disabled by default; an app
/// may explicitly install `.system` or a privacy/proxy-aware loader.
public struct NMPImageLoader {
    let render: (URL) -> AnyView
    let isEnabled: Bool

    public init<Content: View>(@ViewBuilder render: @escaping (URL) -> Content) {
        self.isEnabled = true
        self.render = { AnyView(render($0)) }
    }

    private init(isEnabled: Bool, render: @escaping (URL) -> AnyView) {
        self.isEnabled = isEnabled
        self.render = render
    }

    public static let disabled = NMPImageLoader(isEnabled: false) { _ in
        AnyView(EmptyView())
    }

    public static let system = NMPImageLoader { url in
        AsyncImage(url: url) { phase in
            switch phase {
            case .empty:
                ZStack {
                    Color.secondary.opacity(0.08)
                    ProgressView().controlSize(.small)
                }
            case .success(let image):
                image.resizable().scaledToFill()
            case .failure:
                ZStack {
                    Color.secondary.opacity(0.08)
                    Image(systemName: "photo").foregroundStyle(.secondary)
                }
            @unknown default:
                Color.secondary.opacity(0.08)
            }
        }
    }
}

private struct NMPUIThemeKey: EnvironmentKey {
    static let defaultValue = NMPUITheme()
}

private struct NMPImageLoaderKey: EnvironmentKey {
    static let defaultValue = NMPImageLoader.disabled
}

public extension EnvironmentValues {
    var nmpUITheme: NMPUITheme {
        get { self[NMPUIThemeKey.self] }
        set { self[NMPUIThemeKey.self] = newValue }
    }

    var nmpImageLoader: NMPImageLoader {
        get { self[NMPImageLoaderKey.self] }
        set { self[NMPImageLoaderKey.self] = newValue }
    }
}

public extension View {
    func nmpUITheme(_ theme: NMPUITheme) -> some View {
        environment(\.nmpUITheme, theme)
    }

    func nmpImageLoader(_ loader: NMPImageLoader) -> some View {
        environment(\.nmpImageLoader, loader)
    }
}

struct NMPRemoteImage: View {
    @Environment(\.nmpImageLoader) private var loader
    let url: URL

    var body: some View {
        if loader.isEnabled {
            loader.render(url)
        } else {
            ZStack {
                Color.secondary.opacity(0.08)
                Image(systemName: "photo").foregroundStyle(.secondary)
            }
        }
    }
}
