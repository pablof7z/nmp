import NMPContent
import SwiftUI

public enum NMPUserCardVariant: Sendable, Hashable {
    case featured
    case landscape
    case compact
}

/// Ready-made profile card with genuinely different compositions for feature,
/// list, and dense contexts. Follow state remains controlled by the app.
public struct NMPUserCard: View {
    @Environment(\.nmpUITheme) private var theme
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize

    public let pubkey: String
    public let profile: NostrProfileMetadata?
    public let variant: NMPUserCardVariant
    public let isFollowing: Bool
    public let action: (() -> Void)?
    public let followAction: (() -> Void)?

    public init(
        pubkey: String,
        profile: NostrProfileMetadata? = nil,
        variant: NMPUserCardVariant = .featured,
        isFollowing: Bool = false,
        action: (() -> Void)? = nil,
        followAction: (() -> Void)? = nil
    ) {
        self.pubkey = pubkey
        self.profile = profile
        self.variant = variant
        self.isFollowing = isFollowing
        self.action = action
        self.followAction = followAction
    }

    public var body: some View {
        Group {
            switch variant {
            case .featured: featured
            case .landscape: landscape
            case .compact: compact
            }
        }
        .modifier(NMPCardInteraction(accessibilityName: "Open profile", action: action))
        .accessibilityElement(children: .contain)
    }

    private var featured: some View {
        VStack(alignment: .leading, spacing: 0) {
            banner
                .frame(height: 112)
                .clipped()
                .overlay(alignment: .bottomLeading) {
                    NMPAvatar(pubkey: pubkey, profile: profile, size: 72)
                        .overlay(Circle().stroke(theme.surface, lineWidth: 4))
                        .offset(x: 18, y: 34)
                }

            VStack(alignment: .leading, spacing: 9) {
                HStack(alignment: .top) {
                    VStack(alignment: .leading, spacing: 2) {
                        NMPName(pubkey: pubkey, profile: profile)
                            .font(.title3.weight(.bold))
                        if let nip05 = profile?.nip05, !nip05.isEmpty {
                            NMPNIP05(nip05)
                        }
                    }
                    Spacer(minLength: 10)
                    followButton
                }
                if let about = profile?.about, !about.isEmpty {
                    Text(about)
                        .font(.subheadline)
                        .foregroundStyle(theme.secondary)
                        .lineLimit(dynamicTypeSize.isAccessibilitySize ? nil : 3)
                }
            }
            .padding(.top, 42)
            .padding([.horizontal, .bottom], 18)
        }
        .background(theme.surface, in: RoundedRectangle(cornerRadius: theme.cornerRadius))
        .overlay(RoundedRectangle(cornerRadius: theme.cornerRadius).strokeBorder(theme.border, lineWidth: 0.5))
        .clipShape(RoundedRectangle(cornerRadius: theme.cornerRadius))
    }

    private var landscape: some View {
        HStack(alignment: .top, spacing: 14) {
            NMPAvatar(pubkey: pubkey, profile: profile, size: 58)
            VStack(alignment: .leading, spacing: 5) {
                NMPName(pubkey: pubkey, profile: profile)
                    .font(.headline)
                if let about = profile?.about, !about.isEmpty {
                    Text(about)
                        .font(.subheadline)
                        .foregroundStyle(theme.secondary)
                        .lineLimit(dynamicTypeSize.isAccessibilitySize ? nil : 2)
                }
            }
            Spacer(minLength: 8)
            followButton
        }
        .padding(15)
        .background(theme.surface, in: RoundedRectangle(cornerRadius: 16))
        .overlay(RoundedRectangle(cornerRadius: 16).strokeBorder(theme.border, lineWidth: 0.5))
    }

    private var compact: some View {
        HStack(spacing: 10) {
            NMPAvatar(pubkey: pubkey, profile: profile, size: 38)
            VStack(alignment: .leading, spacing: 1) {
                NMPName(pubkey: pubkey, profile: profile)
                    .font(.subheadline.weight(.semibold))
                Text(NMPDisplayName.abbreviatedPubkey(pubkey))
                    .font(.caption2.monospaced())
                    .foregroundStyle(theme.secondary)
            }
            Spacer(minLength: 8)
            followButton
        }
        .padding(.vertical, 8)
    }

    @ViewBuilder
    private var banner: some View {
        if let banner = profile?.banner, let url = URL(string: banner) {
            NMPRemoteImage(url: url)
        } else {
            ZStack(alignment: .bottomTrailing) {
                NMPAvatar.placeholderColor(for: pubkey).opacity(0.22)
                Image(systemName: "person.2.wave.2")
                    .font(.system(size: 38, weight: .light))
                    .foregroundStyle(NMPAvatar.placeholderColor(for: pubkey).opacity(0.55))
                    .padding(18)
            }
        }
    }

    private var followButton: some View {
        Button(action: { followAction?() }) {
            Text(isFollowing ? "Following" : "Follow")
                .font(.caption.weight(.semibold))
                .padding(.horizontal, 12)
                .padding(.vertical, 7)
                .foregroundStyle(isFollowing ? theme.foreground : Color.white)
                .background(isFollowing ? theme.elevatedSurface : theme.accent, in: Capsule())
        }
        .buttonStyle(.plain)
        .disabled(followAction == nil)
    }
}
