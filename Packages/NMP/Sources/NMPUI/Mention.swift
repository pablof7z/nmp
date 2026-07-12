import NMPContent
import SwiftUI

public enum NMPMentionVariant: Sendable, Hashable {
    case text
    case avatar
    case pill
}

/// A resolved, inline-safe profile mention. Its visual variant and long-press
/// preview are ordinary values, so different call sites in one app can choose
/// different treatments.
public struct NMPProfileMention: View {
    @Environment(\.nmpUITheme) private var theme
    @State private var showsPreview = false

    public let pubkey: String
    public let profile: NostrProfileMetadata?
    public let variant: NMPMentionVariant
    public let showsLongPressPreview: Bool
    public let action: (() -> Void)?

    public init(
        pubkey: String,
        profile: NostrProfileMetadata? = nil,
        variant: NMPMentionVariant = .avatar,
        showsLongPressPreview: Bool = false,
        action: (() -> Void)? = nil
    ) {
        self.pubkey = pubkey
        self.profile = profile
        self.variant = variant
        self.showsLongPressPreview = showsLongPressPreview
        self.action = action
    }

    public var body: some View {
        Button(action: { action?() }) {
            label
        }
        .buttonStyle(.plain)
        .simultaneousGesture(
            LongPressGesture(minimumDuration: 0.45).onEnded { _ in
                guard showsLongPressPreview else { return }
                showsPreview = true
            }
        )
        .popover(isPresented: $showsPreview, attachmentAnchor: .rect(.bounds), arrowEdge: .bottom) {
            NMPMentionPreview(pubkey: pubkey, profile: profile)
        }
        .accessibilityHint(action == nil ? "" : "Opens profile")
    }

    @ViewBuilder
    private var label: some View {
        switch variant {
        case .text:
            Text("@\(NMPDisplayName.resolve(pubkey: pubkey, profile: profile))")
                .font(.body.weight(.medium))
                .foregroundStyle(theme.accent)
        case .avatar:
            HStack(spacing: 5) {
                NMPAvatar(pubkey: pubkey, profile: profile, size: 20)
                Text(NMPDisplayName.resolve(pubkey: pubkey, profile: profile))
                    .lineLimit(1)
            }
            .font(.body.weight(.medium))
            .foregroundStyle(theme.accent)
        case .pill:
            HStack(spacing: 6) {
                NMPAvatar(pubkey: pubkey, profile: profile, size: 22)
                Text(NMPDisplayName.resolve(pubkey: pubkey, profile: profile))
                    .lineLimit(1)
            }
            .font(.subheadline.weight(.semibold))
            .foregroundStyle(theme.foreground)
            .padding(.vertical, 4)
            .padding(.leading, 4)
            .padding(.trailing, 9)
            .background(theme.elevatedSurface, in: Capsule())
            .overlay(Capsule().strokeBorder(theme.border, lineWidth: 0.5))
        }
    }
}

public struct NMPMentionPreview: View {
    @Environment(\.nmpUITheme) private var theme

    public let pubkey: String
    public let profile: NostrProfileMetadata?

    public init(pubkey: String, profile: NostrProfileMetadata? = nil) {
        self.pubkey = pubkey
        self.profile = profile
    }

    public var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            NMPProfileIdentity(pubkey: pubkey, profile: profile, avatarSize: 48)
            if let about = profile?.about, !about.isEmpty {
                Text(about)
                    .font(.subheadline)
                    .foregroundStyle(theme.secondary)
                    .lineLimit(4)
            }
        }
        .padding(16)
        .frame(width: 290, alignment: .leading)
    }
}
