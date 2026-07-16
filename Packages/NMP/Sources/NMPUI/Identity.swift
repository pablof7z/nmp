import NMPContent
import SwiftUI

public enum NMPDisplayName {
    public static func resolve(pubkey: String, profile: NMPProfilePresentation?) -> String {
        let candidates = [profile?.displayName, profile?.name]
        if let value = candidates.compactMap({ $0?.trimmingCharacters(in: .whitespacesAndNewlines) })
            .first(where: { !$0.isEmpty }) {
            return value
        }
        return abbreviatedPubkey(pubkey)
    }

    public static func abbreviatedPubkey(_ pubkey: String) -> String {
        guard pubkey.count > 18 else { return pubkey }
        return "\(pubkey.prefix(10))…\(pubkey.suffix(6))"
    }
}

/// Live-data-independent name primitive used by every higher-level identity
/// component. It remains useful before kind:0 arrives.
public struct NMPName: View {
    public let pubkey: String
    public let profile: NMPProfilePresentation?

    public init(pubkey: String, profile: NMPProfilePresentation? = nil) {
        self.pubkey = pubkey
        self.profile = profile
    }

    public var body: some View {
        Text(NMPDisplayName.resolve(pubkey: pubkey, profile: profile))
            .lineLimit(1)
            .accessibilityLabel("Nostr user \(NMPDisplayName.resolve(pubkey: pubkey, profile: profile))")
    }
}

/// Avatar primitive with a stable pubkey-derived placeholder. Supplying no
/// kind:0, a malformed picture URL, or a failed image never collapses layout.
public struct NMPAvatar: View {
    @Environment(\.nmpImageLoader) private var imageLoader

    public let pubkey: String
    public let profile: NMPProfilePresentation?
    public let size: CGFloat

    public init(pubkey: String, profile: NMPProfilePresentation? = nil, size: CGFloat = 40) {
        self.pubkey = pubkey
        self.profile = profile
        self.size = size
    }

    public var body: some View {
        ZStack {
            Circle().fill(Self.placeholderColor(for: pubkey))
            Text(initials)
                .font(.system(size: max(10, size * 0.31), weight: .semibold, design: .rounded))
                .foregroundStyle(.white.opacity(0.94))

            if let pictureURL, imageLoader.isEnabled {
                imageLoader.render(pictureURL)
                    .frame(width: size, height: size)
                    .clipShape(Circle())
            }
        }
        .frame(width: size, height: size)
        .overlay(Circle().strokeBorder(.white.opacity(0.20), lineWidth: 0.5))
        .accessibilityHidden(true)
    }

    public static func placeholderColor(for pubkey: String) -> Color {
        var hash: UInt64 = 14_695_981_039_346_656_037
        for byte in pubkey.utf8 {
            hash ^= UInt64(byte)
            hash &*= 1_099_511_628_211
        }
        let hue = Double(hash % 360) / 360
        let saturation = 0.50 + Double((hash >> 9) % 18) / 100
        let brightness = 0.58 + Double((hash >> 17) % 16) / 100
        return Color(hue: hue, saturation: saturation, brightness: brightness)
    }

    private var pictureURL: URL? {
        guard let picture = profile?.picture else { return nil }
        return URL(string: picture)
    }

    private var initials: String {
        let value = NMPDisplayName.resolve(pubkey: pubkey, profile: profile)
        let words = value.split(whereSeparator: { $0.isWhitespace })
        if words.count >= 2 {
            return String(words.prefix(2).compactMap(\.first)).uppercased()
        }
        return String(value.prefix(2)).uppercased()
    }
}

public struct NMPProfileIdentity: View {
    @Environment(\.nmpUITheme) private var theme

    public let pubkey: String
    public let profile: NMPProfilePresentation?
    public let avatarSize: CGFloat
    public let showsNIP05: Bool

    public init(
        pubkey: String,
        profile: NMPProfilePresentation? = nil,
        avatarSize: CGFloat = 40,
        showsNIP05: Bool = true
    ) {
        self.pubkey = pubkey
        self.profile = profile
        self.avatarSize = avatarSize
        self.showsNIP05 = showsNIP05
    }

    public var body: some View {
        HStack(spacing: 10) {
            NMPAvatar(pubkey: pubkey, profile: profile, size: avatarSize)
            VStack(alignment: .leading, spacing: 1) {
                NMPName(pubkey: pubkey, profile: profile)
                    .font(.subheadline.weight(.semibold))
                    .foregroundStyle(theme.foreground)
                if showsNIP05, let nip05 = profile?.nip05, !nip05.isEmpty {
                    NMPNIP05(nip05)
                }
            }
        }
        .accessibilityElement(children: .combine)
    }
}
