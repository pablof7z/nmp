import Foundation
import NMPContent
import SwiftUI

public enum NMPReactionButtonVariant: Sendable, Hashable {
    case heart
    case spark
    case minimal
}

public enum NMPMotionPreference: Sendable, Hashable {
    case system
    case reduced
    case full
}

public enum NMPCompactCount {
    public static func string(for count: Int) -> String {
        if count >= 1_000 { return String(format: "%.1fk", Double(count) / 1_000) }
        return "\(count)"
    }
}

/// Controlled reaction button. The animation is local presentation state;
/// whether the user reacted and what gets published remain app-owned values.
public struct NMPReactionButton: View {
    @Environment(\.nmpUITheme) private var theme
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var pulse = false

    public let isReacted: Bool
    public let count: Int
    public let variant: NMPReactionButtonVariant
    public let motion: NMPMotionPreference
    public let action: () -> Void

    public init(
        isReacted: Bool,
        count: Int = 0,
        variant: NMPReactionButtonVariant = .heart,
        motion: NMPMotionPreference = .system,
        action: @escaping () -> Void
    ) {
        self.isReacted = isReacted
        self.count = count
        self.variant = variant
        self.motion = motion
        self.action = action
    }

    public var body: some View {
        Button {
            if usesReducedMotion {
                pulse = false
                action()
            } else {
                action()
                withAnimation(.spring(response: 0.28, dampingFraction: 0.48)) {
                    pulse = true
                }
                Task { @MainActor in
                    await Task.yield()
                    withAnimation(.spring(response: 0.32, dampingFraction: 0.72)) {
                        pulse = false
                    }
                }
            }
        } label: {
            switch variant {
            case .heart:
                HStack(spacing: 6) {
                    Image(systemName: isReacted ? "heart.fill" : "heart")
                        .foregroundStyle(isReacted ? Color.pink : theme.secondary)
                        .scaleEffect(pulse ? 1.42 : 1)
                    if count > 0 { countLabel }
                }
                .padding(.horizontal, 11)
                .padding(.vertical, 8)
                .background(isReacted ? Color.pink.opacity(0.10) : theme.surface, in: Capsule())
            case .spark:
                ZStack(alignment: .topTrailing) {
                    Image(systemName: isReacted ? "bolt.heart.fill" : "bolt.heart")
                        .font(.title3.weight(.semibold))
                        .foregroundStyle(isReacted ? theme.accent : theme.secondary)
                        .rotationEffect(.degrees(pulse ? -10 : 0))
                        .scaleEffect(pulse ? 1.34 : 1)
                        .frame(width: 38, height: 38)
                        .background(theme.surface, in: Circle())
                    if count > 0 {
                        Text(compactCount)
                            .font(.system(size: 9, weight: .bold, design: .rounded))
                            .foregroundStyle(.white)
                            .padding(4)
                            .background(theme.accent, in: Capsule())
                            .offset(x: 8, y: -5)
                    }
                }
            case .minimal:
                HStack(spacing: 5) {
                    Image(systemName: isReacted ? "hand.thumbsup.fill" : "hand.thumbsup")
                        .scaleEffect(pulse ? 1.28 : 1)
                    if count > 0 { countLabel }
                }
                .font(.subheadline)
                .foregroundStyle(isReacted ? theme.accent : theme.secondary)
            }
        }
        .buttonStyle(.plain)
        .accessibilityLabel(isReacted ? "Remove reaction" : "React")
        .accessibilityValue("\(count) reactions")
    }

    private var countLabel: some View {
        Text(compactCount).font(.caption.weight(.semibold))
    }

    private var compactCount: String {
        NMPCompactCount.string(for: count)
    }

    private var usesReducedMotion: Bool {
        switch motion {
        case .system: reduceMotion
        case .reduced: true
        case .full: false
        }
    }
}

public struct NMPReactionPerson: Identifiable, Hashable, Sendable {
    public let pubkey: String
    public let profile: NostrProfileMetadata?

    public var id: String { pubkey }

    public init(pubkey: String, profile: NostrProfileMetadata? = nil) {
        self.pubkey = pubkey
        self.profile = profile
    }
}

/// Social-proof treatment inspired by avatar-backed reactions: visible people
/// are the control, with overflow and current-user state alongside them.
public struct NMPAvatarReactionButton: View {
    @Environment(\.nmpUITheme) private var theme

    public let people: [NMPReactionPerson]
    public let totalCount: Int
    public let isReacted: Bool
    public let action: () -> Void

    public init(
        people: [NMPReactionPerson],
        totalCount: Int,
        isReacted: Bool,
        action: @escaping () -> Void
    ) {
        self.people = people
        self.totalCount = totalCount
        self.isReacted = isReacted
        self.action = action
    }

    public var body: some View {
        Button(action: action) {
            HStack(spacing: 8) {
                if people.isEmpty {
                    Image(systemName: "person.crop.circle.badge.plus")
                        .font(.title3)
                        .foregroundStyle(theme.secondary)
                } else {
                    NMPAvatarGroup(
                        people: people.map { NMPAvatarItem(pubkey: $0.pubkey, profile: $0.profile) },
                        maximumVisible: 4,
                        size: 28
                    )
                }

                Text(totalCount == 0 ? "Be the first" : "\(totalCount) reacted")
                    .font(.caption.weight(.medium))
                    .foregroundStyle(theme.secondary)

                Image(systemName: isReacted ? "heart.fill" : "heart")
                    .foregroundStyle(isReacted ? Color.pink : theme.secondary)
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 7)
            .background(theme.surface, in: Capsule())
            .overlay(Capsule().strokeBorder(theme.border, lineWidth: 0.5))
        }
        .buttonStyle(.plain)
        .accessibilityLabel(isReacted ? "Remove reaction" : "React")
        .accessibilityValue("\(totalCount) reactions")
    }
}

public struct NMPEmojiReaction: Identifiable, Hashable, Sendable {
    public let emoji: String
    public let count: Int
    public let isSelected: Bool

    public var id: String { emoji }

    public init(emoji: String, count: Int, isSelected: Bool = false) {
        self.emoji = emoji
        self.count = count
        self.isSelected = isSelected
    }
}

/// Slack-like grouped reaction treatment for events where one generic heart
/// loses useful intent. The app maps each selected emoji to its own write.
public struct NMPEmojiReactionBar: View {
    @Environment(\.nmpUITheme) private var theme

    public let reactions: [NMPEmojiReaction]
    public let select: (String) -> Void
    public let add: (() -> Void)?

    public init(
        reactions: [NMPEmojiReaction],
        select: @escaping (String) -> Void,
        add: (() -> Void)? = nil
    ) {
        self.reactions = reactions
        self.select = select
        self.add = add
    }

    public var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 7) {
                ForEach(reactions) { reaction in
                    Button { select(reaction.emoji) } label: {
                        HStack(spacing: 5) {
                            Text(reaction.emoji)
                            Text("\(reaction.count)")
                                .font(.caption2.weight(.semibold))
                        }
                        .padding(.horizontal, 9)
                        .padding(.vertical, 6)
                        .background(
                            reaction.isSelected ? theme.accent.opacity(0.13) : theme.surface,
                            in: Capsule()
                        )
                        .overlay(
                            Capsule().strokeBorder(
                                reaction.isSelected ? theme.accent.opacity(0.65) : theme.border,
                                lineWidth: 0.75
                            )
                        )
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel(
                        reaction.isSelected
                            ? "Remove \(reaction.emoji) reaction"
                            : "React with \(reaction.emoji)"
                    )
                    .accessibilityValue("\(reaction.count) reactions")
                }
                if let add {
                    Button(action: add) {
                        Image(systemName: "face.smiling.inverse")
                            .frame(width: 30, height: 30)
                            .foregroundStyle(theme.secondary)
                            .background(theme.surface, in: Circle())
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel("Add reaction")
                }
            }
        }
    }
}
