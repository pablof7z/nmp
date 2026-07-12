import SwiftUI

enum NMPFlowRole: Hashable {
    case inline
    case block
    case breakLine
}

struct NMPFlowRoleKey: LayoutValueKey {
    static let defaultValue = NMPFlowRole.inline
}

/// A small native flow layout is what lets a resolved mention, app-defined
/// custom event view, or compact card participate in authored text without
/// flattening it back into an attributed string.
struct NMPFlowLayout: Layout {
    var horizontalSpacing: CGFloat = 0
    var verticalSpacing: CGFloat = 5

    func sizeThatFits(
        proposal: ProposedViewSize,
        subviews: Subviews,
        cache: inout ()
    ) -> CGSize {
        arrangement(proposal: proposal, subviews: subviews).size
    }

    func placeSubviews(
        in bounds: CGRect,
        proposal: ProposedViewSize,
        subviews: Subviews,
        cache: inout ()
    ) {
        let result = arrangement(
            proposal: ProposedViewSize(width: bounds.width, height: proposal.height),
            subviews: subviews
        )
        for (index, frame) in result.frames.enumerated() {
            subviews[index].place(
                at: CGPoint(x: bounds.minX + frame.minX, y: bounds.minY + frame.minY),
                anchor: .topLeading,
                proposal: ProposedViewSize(width: frame.width, height: frame.height)
            )
        }
    }

    private func arrangement(proposal: ProposedViewSize, subviews: Subviews) -> Result {
        let maximumWidth = max(1, proposal.width ?? 640)
        var frames = Array(repeating: CGRect.zero, count: subviews.count)
        var x: CGFloat = 0
        var y: CGFloat = 0
        var lineHeight: CGFloat = 0
        var usedWidth: CGFloat = 0

        func nextLine() -> (CGFloat, CGFloat, CGFloat) {
            (0, y + lineHeight + (lineHeight > 0 ? verticalSpacing : 0), 0)
        }

        for index in subviews.indices {
            let subview = subviews[index]
            let role = subview[NMPFlowRoleKey.self]

            if role == .breakLine {
                (x, y, lineHeight) = nextLine()
                frames[index] = CGRect(x: x, y: y, width: 0, height: 0)
                continue
            }

            if role == .block {
                if x > 0 || lineHeight > 0 {
                    (x, y, lineHeight) = nextLine()
                }
                let dimensions = subview.dimensions(
                    in: ProposedViewSize(width: maximumWidth, height: nil)
                )
                let width = min(maximumWidth, max(0, dimensions.width))
                frames[index] = CGRect(x: 0, y: y, width: width, height: dimensions.height)
                usedWidth = max(usedWidth, width)
                y += dimensions.height + verticalSpacing
                x = 0
                lineHeight = 0
                continue
            }

            var dimensions = subview.dimensions(in: .unspecified)
            if dimensions.width > maximumWidth {
                dimensions = subview.dimensions(
                    in: ProposedViewSize(width: maximumWidth, height: nil)
                )
            }
            if x > 0, x + horizontalSpacing + dimensions.width > maximumWidth {
                (x, y, lineHeight) = nextLine()
            }
            let leadingSpacing = x > 0 ? horizontalSpacing : 0
            x += leadingSpacing
            frames[index] = CGRect(x: x, y: y, width: dimensions.width, height: dimensions.height)
            x += dimensions.width
            lineHeight = max(lineHeight, dimensions.height)
            usedWidth = max(usedWidth, x)
        }

        return Result(
            frames: frames,
            size: CGSize(width: min(maximumWidth, usedWidth), height: y + lineHeight)
        )
    }

    private struct Result {
        var frames: [CGRect]
        var size: CGSize
    }
}
