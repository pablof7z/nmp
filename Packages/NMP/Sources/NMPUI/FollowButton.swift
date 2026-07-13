import NMP
import SwiftUI

public enum NMPFollowButtonVariant: Sendable, Hashable {
    case compact
    case prominent
    case icon
}

/// The controlled visual primitive. It renders only an NMP-projected
/// snapshot plus operation progress and forwards a tap. This makes every
/// state independently previewable without recreating NIP-02 logic.
public struct NMPFollowButtonBody: View {
    @Environment(\.nmpUITheme) private var theme
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var confirmationScale: CGFloat = 1

    public let snapshot: NMPFollowingSnapshot
    public let isActing: Bool
    public let offersAnotherAttempt: Bool
    public let variant: NMPFollowButtonVariant
    public let action: () -> Void

    public init(
        snapshot: NMPFollowingSnapshot,
        isActing: Bool = false,
        offersAnotherAttempt: Bool = false,
        variant: NMPFollowButtonVariant = .compact,
        action: @escaping () -> Void
    ) {
        self.snapshot = snapshot
        self.isActing = isActing
        self.offersAnotherAttempt = offersAnotherAttempt
        self.variant = variant
        self.action = action
    }

    public var body: some View {
        Button(action: action) {
            label
                .scaleEffect(confirmationScale)
                .frame(maxWidth: variant == .prominent ? .infinity : nil)
                .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .disabled(!canAct)
        .opacity(isUnavailable ? 0.62 : 1)
        .accessibilityLabel(accessibilityLabel)
        .accessibilityValue(accessibilityValue)
        .accessibilityHint(accessibilityHint)
        .onChange(of: snapshot.relationship) { relationship in
            guard relationship != .unknown, !reduceMotion else { return }
            withAnimation(.spring(response: 0.26, dampingFraction: 0.54)) {
                confirmationScale = 1.08
            }
            withAnimation(.spring(response: 0.34, dampingFraction: 0.78).delay(0.08)) {
                confirmationScale = 1
            }
        }
    }

    @ViewBuilder
    private var label: some View {
        switch variant {
        case .compact:
            HStack(spacing: 6) {
                statusGlyph
                Text(title)
                    .font(.caption.weight(.semibold))
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 7)
            .foregroundStyle(foreground)
            .background(background, in: Capsule())
            .overlay(Capsule().strokeBorder(border, lineWidth: 0.75))

        case .prominent:
            HStack(spacing: 8) {
                statusGlyph
                Text(title)
                    .font(.subheadline.weight(.semibold))
            }
            .padding(.horizontal, 18)
            .padding(.vertical, 11)
            .foregroundStyle(foreground)
            .background(background, in: Capsule())
            .overlay(Capsule().strokeBorder(border, lineWidth: 0.75))

        case .icon:
            statusGlyph
                .font(.subheadline.weight(.semibold))
                .frame(width: 38, height: 38)
                .foregroundStyle(foreground)
                .background(background, in: Circle())
                .overlay(Circle().strokeBorder(border, lineWidth: 0.75))
        }
    }

    @ViewBuilder
    private var statusGlyph: some View {
        if isActing || snapshot.availability == .acquiring {
            ProgressView()
                .controlSize(.small)
                .tint(foreground)
        } else {
            Image(systemName: symbol)
        }
    }

    private var title: String {
        if offersAnotherAttempt { return "Retry" }
        switch snapshot.relationship {
        case .following: return "Following"
        case .notFollowing, .unknown: return "Follow"
        }
    }

    private var symbol: String {
        if offersAnotherAttempt { return "arrow.clockwise" }
        switch snapshot.availability {
        case .cachedOnly, .sourceUnavailable: return "wifi.exclamationmark"
        case .noContactList: return "person.crop.circle.badge.exclamationmark"
        case .signedOut: return "person.crop.circle.badge.exclamationmark"
        case .acquiring: return "plus"
        case .ready:
            switch snapshot.relationship {
            case .following: return "checkmark"
            case .notFollowing, .unknown: return "plus"
            }
        }
    }

    private var canAct: Bool {
        snapshot.availability == .ready
            && snapshot.relationship != .unknown
            && !isActing
    }

    private var isUnavailable: Bool {
        snapshot.availability != .ready
    }

    private var foreground: Color {
        if offersAnotherAttempt && !isUnavailable {
            return .white
        }
        if snapshot.relationship == .following || isUnavailable {
            return theme.foreground
        }
        return .white
    }

    private var background: Color {
        if isUnavailable {
            return theme.elevatedSurface
        }
        if offersAnotherAttempt {
            return theme.accent
        }
        return snapshot.relationship == .following ? theme.elevatedSurface : theme.accent
    }

    private var border: Color {
        if offersAnotherAttempt && !isUnavailable { return .clear }
        return snapshot.relationship == .following || isUnavailable ? theme.border : .clear
    }

    private var accessibilityLabel: String {
        if offersAnotherAttempt {
            return snapshot.relationship == .following ? "Retry unfollow" : "Retry follow"
        }
        return snapshot.relationship == .following ? "Unfollow" : "Follow"
    }

    private var accessibilityValue: String {
        if offersAnotherAttempt && canAct { return "Ready to retry" }
        switch snapshot.availability {
        case .signedOut: return "Signed out"
        case .acquiring: return "Loading current follow state"
        case .ready: return snapshot.relationship == .following ? "Following" : "Not following"
        case .noContactList: return "No contact list"
        case .cachedOnly: return "Cached state only"
        case .sourceUnavailable: return "Follow sources unavailable"
        }
    }

    private var accessibilityHint: String {
        if offersAnotherAttempt && canAct {
            return "Retries the NMP follow action from the latest canonical state"
        }
        switch snapshot.availability {
        case .signedOut: return "Sign in to change this relationship"
        case .acquiring: return "Wait for NMP to resolve the current contact list"
        case .ready: return snapshot.relationship == .following ? "Stops following this user" : "Follows this user"
        case .noContactList: return "Create a contact list before using the ordinary follow action"
        case .cachedOnly: return "Reconnect before changing the contact list"
        case .sourceUnavailable: return "NMP has no safe source plan for this edit"
        }
    }
}

/// Ready-made connected component. `NMPFollowing` owns the NMP observation
/// and invokes NMP's typed action; this view owns only pixels, accessibility,
/// and the confirmation animation.
public struct NMPFollowButton: View {
    @ObservedObject private var following: NMPFollowing
    public let variant: NMPFollowButtonVariant

    public init(
        following: NMPFollowing,
        variant: NMPFollowButtonVariant = .compact
    ) {
        self.following = following
        self.variant = variant
    }

    public var body: some View {
        NMPFollowButtonBody(
            snapshot: following.snapshot,
            isActing: following.isActing,
            offersAnotherAttempt: following.offersAnotherAttempt,
            variant: variant,
            action: following.performPrimaryAction
        )
    }
}
