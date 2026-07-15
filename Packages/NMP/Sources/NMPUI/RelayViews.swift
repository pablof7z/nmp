import Foundation
import NMP
import SwiftUI

/// The presentation subset of one engine-owned NIP-11 snapshot. This is an
/// immutable view value, not a cache: the caller still owns when to invoke
/// the public relay-information one-shot and passes each result down.
public struct NMPRelayPresentation: Sendable, Equatable {
    public let relay: String
    public let advertisedName: String?
    public let advertisedDescription: String?
    /// The exact advertised icon string. NMPUI never dereferences it; an app
    /// may apply its own trust/network policy and supply an already-loaded
    /// native `Image` to the controlled icon or list-entry primitive.
    public let advertisedIcon: String?
    public let freshness: RelayInformationFreshness
    public let lastError: String?

    public init(_ information: RelayInformation) {
        self.init(
            relay: information.relay,
            advertisedName: information.document.name,
            advertisedDescription: information.document.description,
            advertisedIcon: information.document.icon,
            freshness: information.freshness,
            lastError: information.lastError.map(String.init(describing:))
        )
    }

    /// Deterministic preview/fixture initializer. It makes no acquisition or
    /// persistence claim and never inserts this value into NMP's cache.
    public init(
        relay: String,
        advertisedName: String? = nil,
        advertisedDescription: String? = nil,
        advertisedIcon: String? = nil,
        freshness: RelayInformationFreshness,
        lastError: String? = nil
    ) {
        self.relay = relay
        self.advertisedName = advertisedName
        self.advertisedDescription = advertisedDescription
        self.advertisedIcon = advertisedIcon
        self.freshness = freshness
        self.lastError = lastError
    }

    public var displayName: String {
        Self.nonempty(advertisedName) ?? Self.relayHost(relay) ?? relay
    }

    public var displayDescription: String {
        Self.nonempty(advertisedDescription) ?? "No relay description provided"
    }

    public var initials: String {
        let words = displayName.split(whereSeparator: { $0.isWhitespace })
        if words.count >= 2 {
            return String(words.prefix(2).compactMap(\.first)).uppercased()
        }
        return String(displayName.prefix(2)).uppercased()
    }

    private static func nonempty(_ value: String?) -> String? {
        let trimmed = value?.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed?.isEmpty == false ? trimmed : nil
    }

    private static func relayHost(_ relay: String) -> String? {
        guard let host = URLComponents(string: relay)?.host else { return nil }
        return nonempty(host)
    }
}

/// Caller-owned operation state around the one-shot relay-information value.
/// Loading and failure are explicit; an available stale snapshot remains
/// renderable and retains its separate `lastError` evidence.
public enum NMPRelayInformationState: Sendable, Equatable {
    case loading(relay: String)
    case available(NMPRelayPresentation)
    case unavailable(relay: String, reason: String?)

    public init(_ information: RelayInformation) {
        self = .available(NMPRelayPresentation(information))
    }

    public var relay: String {
        switch self {
        case .loading(let relay), .unavailable(let relay, _): relay
        case .available(let presentation): presentation.relay
        }
    }

    public var presentation: NMPRelayPresentation? {
        guard case .available(let presentation) = self else { return nil }
        return presentation
    }

    public var displayName: String {
        presentation?.displayName
            ?? NMPRelayPresentation(
                relay: relay,
                freshness: .stale
            ).displayName
    }

    public var displayDescription: String {
        switch self {
        case .loading:
            "Relay information is loading"
        case .available(let presentation):
            presentation.displayDescription
        case .unavailable(_, let reason):
            Self.nonempty(reason) ?? "Relay information unavailable"
        }
    }

    public var informationLabel: String {
        switch self {
        case .loading: "Loading"
        case .available(let presentation):
            presentation.freshness == .fresh ? "Fresh" : "Stale"
        case .unavailable: "Unavailable"
        }
    }

    public var lastError: String? {
        switch self {
        case .available(let presentation): presentation.lastError
        case .unavailable(_, let reason): Self.nonempty(reason)
        case .loading: nil
        }
    }

    public var initials: String {
        presentation?.initials ?? String(displayName.prefix(2)).uppercased()
    }

    private static func nonempty(_ value: String?) -> String? {
        let trimmed = value?.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed?.isEmpty == false ? trimmed : nil
    }
}

/// Presentation of the context-scoped `SourceStatus` supplied by a query.
/// There is deliberately no URL-global connected/authenticated/reconnecting
/// case: those facts are not present on NMP's public runtime projection.
public enum NMPRelayRuntimePresentation: Sendable, Hashable {
    case statusUnavailable
    case requesting
    case connecting
    case disconnected
    case awaitingAuth(phase: AuthPhase)
    case authDenied
    case error

    public init(_ status: SourceStatus?) {
        switch status {
        case nil: self = .statusUnavailable
        case .requesting: self = .requesting
        case .connecting: self = .connecting
        case .disconnected: self = .disconnected
        case .awaitingAuth(let phase): self = .awaitingAuth(phase: phase)
        case .authDenied: self = .authDenied
        case .error: self = .error
        }
    }

    public var label: String {
        switch self {
        case .statusUnavailable: "Status unavailable"
        case .requesting: "Requesting"
        case .connecting: "Connecting"
        case .disconnected: "Disconnected"
        case .awaitingAuth(let phase):
            switch phase {
            case .awaitingPolicy: "Awaiting authentication policy"
            case .awaitingSignature: "Awaiting authentication signature"
            }
        case .authDenied: "Authentication denied"
        case .error: "Connection error"
        }
    }
}

/// Controlled relay icon. The component never loads the advertised icon URL:
/// pass an already-resolved native image after applying app-owned media policy.
public struct NMPRelayIcon: View {
    public let state: NMPRelayInformationState
    public let image: Image?
    public let size: CGFloat

    public init(
        state: NMPRelayInformationState,
        image: Image? = nil,
        size: CGFloat = 44
    ) {
        self.state = state
        self.image = image
        self.size = size
    }

    public var body: some View {
        ZStack {
            Circle().fill(NMPAvatar.placeholderColor(for: state.relay))
            Text(state.initials)
                .font(.system(size: max(10, size * 0.30), weight: .semibold, design: .rounded))
                .foregroundStyle(.white.opacity(0.94))
            if let image {
                image
                    .resizable()
                    .scaledToFill()
                    .frame(width: size, height: size)
                    .clipShape(Circle())
            }
        }
        .frame(width: size, height: size)
        .accessibilityHidden(true)
    }
}

public struct NMPRelayName: View {
    public let state: NMPRelayInformationState

    public init(state: NMPRelayInformationState) {
        self.state = state
    }

    public var body: some View {
        Text(state.displayName)
            .lineLimit(1)
            .accessibilityLabel("Relay \(state.displayName)")
    }
}

public struct NMPRelayDescription: View {
    public let state: NMPRelayInformationState

    public init(state: NMPRelayInformationState) {
        self.state = state
    }

    public var body: some View {
        Text(state.displayDescription)
            .lineLimit(2)
            .accessibilityLabel(state.displayDescription)
    }
}

public struct NMPRelayRuntimeStatus: View {
    @Environment(\.nmpUITheme) private var theme
    public let status: NMPRelayRuntimePresentation

    public init(status: NMPRelayRuntimePresentation) {
        self.status = status
    }

    public var body: some View {
        Text(status.label)
            .font(.caption2.weight(.semibold))
            .foregroundStyle(theme.secondary)
            .padding(.horizontal, 8)
            .padding(.vertical, 4)
            .background(theme.elevatedSurface, in: Capsule())
            .accessibilityLabel("Relay runtime status: \(status.label)")
    }
}

/// Controlled relay list/menu row. It composes supplied one-shot information,
/// context-scoped runtime evidence, and an optional already-loaded image. It
/// cannot acquire metadata or open/close a relay connection.
public struct NMPRelayListEntry: View {
    @Environment(\.nmpUITheme) private var theme

    public let information: NMPRelayInformationState
    public let runtime: NMPRelayRuntimePresentation
    public let image: Image?
    public let action: (() -> Void)?

    public init(
        information: NMPRelayInformationState,
        runtime: NMPRelayRuntimePresentation = .statusUnavailable,
        image: Image? = nil,
        action: (() -> Void)? = nil
    ) {
        self.information = information
        self.runtime = runtime
        self.image = image
        self.action = action
    }

    public var body: some View {
        NMPActionSurface(action: action) {
            HStack(alignment: .top, spacing: 12) {
                NMPRelayIcon(state: information, image: image, size: 44)
                VStack(alignment: .leading, spacing: 4) {
                    NMPRelayName(state: information)
                        .font(.subheadline.weight(.semibold))
                        .foregroundStyle(theme.foreground)
                    NMPRelayDescription(state: information)
                        .font(.caption)
                        .foregroundStyle(theme.secondary)
                    HStack(spacing: 6) {
                        Text(information.informationLabel)
                            .font(.caption2.weight(.semibold))
                            .foregroundStyle(theme.secondary)
                        NMPRelayRuntimeStatus(status: runtime)
                    }
                    if let error = information.lastError {
                        Text(error)
                            .font(.caption2)
                            .foregroundStyle(theme.secondary)
                            .lineLimit(2)
                            .accessibilityLabel("Relay information error: \(error)")
                    }
                }
                Spacer(minLength: 0)
            }
            .padding(12)
            .background(theme.surface, in: RoundedRectangle(cornerRadius: theme.cornerRadius))
            .overlay(
                RoundedRectangle(cornerRadius: theme.cornerRadius)
                    .stroke(theme.border, lineWidth: 1)
            )
            .accessibilityElement(children: .combine)
            .accessibilityLabel(accessibilityLabel)
        }
    }

    var accessibilityLabel: String {
        let parts: [String?] = [
            "Relay \(information.displayName)",
            information.displayDescription,
            "Relay information \(information.informationLabel)",
            runtime.label,
            information.lastError.map { "Relay information error: \($0)" },
        ]
        return parts.compactMap { $0 }.joined(separator: ". ")
    }
}
