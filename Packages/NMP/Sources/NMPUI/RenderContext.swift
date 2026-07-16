import Foundation

/// Immutable ancestry threaded through nested content rendering. It is a
/// presentation value, not a shared budget owner or acquisition coordinator.
public struct NostrContentRenderContext: Sendable, Hashable {
    public let ancestorTargetKeys: [String]
    public let depth: Int
    public let maximumDepth: Int

    public static let root = NostrContentRenderContext(
        ancestorTargetKeys: [],
        depth: 0,
        maximumDepth: 3
    )

    public init(
        ancestorTargetKeys: [String],
        depth: Int,
        maximumDepth: Int
    ) {
        self.ancestorTargetKeys = ancestorTargetKeys
        self.depth = max(0, depth)
        self.maximumDepth = max(0, maximumDepth)
    }

    /// Returns the next immutable context, or `nil` when the target would
    /// create a cycle or exceed the configured presentation depth.
    public func descending(into targetKey: String) -> NostrContentRenderContext? {
        guard depth < maximumDepth, !ancestorTargetKeys.contains(targetKey) else {
            return nil
        }
        return NostrContentRenderContext(
            ancestorTargetKeys: ancestorTargetKeys + [targetKey],
            depth: depth + 1,
            maximumDepth: maximumDepth
        )
    }
}
