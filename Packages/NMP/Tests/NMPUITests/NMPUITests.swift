@testable import NMP
import NMPContent
@testable import NMPUI
import SwiftUI
import XCTest

@MainActor
final class NMPUITests: XCTestCase {
    private let pubkey = "aa4fc8665f5696e33db7e1a572e3b0f5b3d615837b0f362dcb1c8068b098c7b4"

    func testNameFallsBackAndPrefersDisplayName() {
        XCTAssertEqual(NMPDisplayName.abbreviatedPubkey(pubkey), "aa4fc8665f…98c7b4")
        XCTAssertEqual(NMPDisplayName.resolve(pubkey: pubkey, profile: nil), "aa4fc8665f…98c7b4")

        let profile = NMPProfilePresentation(name: "alice", displayName: "Alice A.")
        XCTAssertEqual(NMPDisplayName.resolve(pubkey: pubkey, profile: profile), "Alice A.")
    }

    func testReadingTimeIsBoundedAndConfigurable() {
        XCTAssertEqual(NMPReadingTime.minutes(for: ""), 1)
        XCTAssertEqual(
            NMPReadingTime.minutes(
                for: Array(repeating: "word", count: 440).joined(separator: " ")
            ),
            2
        )
        XCTAssertEqual(NMPReadingTime.minutes(for: "one two three", wordsPerMinute: 2), 2)
    }

    func testLiteralVisibleProfileAndEventComponentsOpenZeroHandles() {
        let probe = ObservationProbe()
        let renderers = NostrContentRenderers.literalReferences
        let profile = occurrence(target: .profile(pubkey: pubkey), original: "npub-literal")
        let event = occurrence(
            target: .event(id: String(repeating: "cd", count: 32), kindHint: 30_023),
            original: "nevent-literal"
        )

        _ = renderers.renderProfileReference(
            NMPProfileReferenceInput(
                occurrence: profile,
                pubkey: pubkey,
                purpose: .body,
                observationFactory: probe.factory,
                actions: NostrContentActions()
            )
        )
        let inlineNode = renderers.renderEventReference(
            NMPEventReferenceInput(
                occurrence: event,
                purpose: .body,
                context: .root,
                observationFactory: probe.factory,
                renderers: renderers,
                actions: NostrContentActions()
            )
        )

        XCTAssertEqual(probe.totalOpenCount, 0)
        XCTAssertEqual(probe.activeCount, 0)
        XCTAssertEqual(inlineNode.layout, .inline)
    }

    func testChangedTargetAtSameOccurrenceGetsDifferentSwiftUIIdentity() {
        let first = occurrence(
            target: .event(id: String(repeating: "cd", count: 32)),
            original: "first"
        )
        let second = occurrence(
            target: .event(id: String(repeating: "ef", count: 32)),
            original: "second"
        )

        XCTAssertNotEqual(
            NostrContent.referenceFragmentID(blockID: 9, occurrence: first),
            NostrContent.referenceFragmentID(blockID: 9, occurrence: second)
        )
    }

    func testEqualVisibleComponentsOwnIndependentHandlesAndReleaseOnlyTheirOwn() {
        let probe = ObservationProbe()
        let target = NostrReferenceTarget.profile(pubkey: pubkey)
        let first = NMPVisibleReferenceObservation(target: target, factory: probe.factory)
        let second = NMPVisibleReferenceObservation(target: target, factory: probe.factory)

        first.appear()
        second.appear()
        XCTAssertEqual(probe.activeCount, 2)

        first.disappear()
        XCTAssertEqual(probe.activeCount, 1)

        second.disappear()
        XCTAssertEqual(probe.activeCount, 0)
    }

    func testProfileReplacementIsLiveAndLastSnapshotSurvivesScrollAwayAndReturn() {
        let probe = ObservationProbe()
        let observation = NMPVisibleReferenceObservation(
            target: .profile(pubkey: pubkey),
            factory: probe.factory
        )
        observation.appear()

        let first = row(idByte: "01", kind: 0, pubkey: pubkey)
        let replacement = row(idByte: "02", kind: 0, pubkey: pubkey)
        probe.send(RowBatch(rows: [first]), to: 0)
        XCTAssertEqual(observation.canonical?.rows.first?.id, first.id)
        probe.send(RowBatch(rows: [replacement]), to: 0)
        XCTAssertEqual(observation.canonical?.rows.first?.id, replacement.id)

        observation.disappear()
        XCTAssertEqual(observation.canonical?.rows.first?.id, replacement.id)

        observation.appear()
        XCTAssertEqual(probe.activeCount, 1)
        XCTAssertEqual(
            observation.canonical?.rows.first?.id,
            replacement.id,
            "reacquiring must not clear the last rendered snapshot"
        )
        observation.disappear()
    }

    func testCanonicalAndHelperEvidenceStayOnTheirExactHandles() {
        let probe = ObservationProbe()
        let observation = NMPVisibleReferenceObservation(
            target: .event(
                id: String(repeating: "cd", count: 32),
                relayHints: ["wss://relay.damus.io"]
            ),
            factory: probe.factory
        )
        observation.appear()
        XCTAssertEqual(probe.activeCount, 2)

        let canonical = row(idByte: "03", kind: 1, pubkey: pubkey)
        let helper = row(idByte: "04", kind: 1, pubkey: pubkey)
        probe.send(RowBatch(rows: [canonical]), to: 0)
        probe.send(RowBatch(rows: [helper]), to: 1)

        XCTAssertEqual(observation.canonical?.rows.first?.id, canonical.id)
        XCTAssertEqual(observation.helpers[0]?.rows.first?.id, helper.id)
        observation.disappear()
        XCTAssertEqual(probe.activeCount, 0)
    }

    func testVisibilityThrashLeaksNoHandlesAndKeepsTheLastSnapshot() {
        let probe = ObservationProbe()
        let observation = NMPVisibleReferenceObservation(
            target: .event(id: String(repeating: "cd", count: 32)),
            factory: probe.factory
        )
        observation.appear()
        let event = row(idByte: "05", kind: 1, pubkey: pubkey)
        probe.send(RowBatch(rows: [event]), to: 0)
        observation.disappear()

        for _ in 0..<100 {
            observation.appear()
            observation.disappear()
        }

        XCTAssertEqual(probe.activeCount, 0)
        XCTAssertEqual(probe.totalOpenCount, probe.totalCancelCount)
        XCTAssertEqual(observation.canonical?.rows.first?.id, event.id)
    }

    func testActualKindAndPurposeDispatchIgnoreMisleadingHintAndReachFallback() {
        let recorder = RendererRecorder()
        let occurrence = occurrence(
            target: .event(
                id: String(repeating: "cd", count: 32),
                kindHint: 30_023
            ),
            original: "misleading-nevent"
        )
        let renderers = NostrContentRenderers()
            .event(kind: 1) { _ in recorder.view("note") }
            .event(kind: 1, purpose: .card) { _ in recorder.view("note-card") }
            .event(kind: 30_023) { _ in recorder.view("article") }
            .fallbackEvent { input in recorder.view("fallback-\(input.event.kind)") }

        _ = renderers.renderEvent(
            eventInput(occurrence: occurrence, row: row(idByte: "06", kind: 1), purpose: .body, renderers: renderers)
        )
        _ = renderers.renderEvent(
            eventInput(occurrence: occurrence, row: row(idByte: "07", kind: 1), purpose: .card, renderers: renderers)
        )
        _ = renderers.renderEvent(
            eventInput(occurrence: occurrence, row: row(idByte: "08", kind: 30_023), purpose: .body, renderers: renderers)
        )
        _ = renderers.renderEvent(
            eventInput(occurrence: occurrence, row: row(idByte: "09", kind: 55_555), purpose: .body, renderers: renderers)
        )

        XCTAssertEqual(recorder.values, ["note", "note-card", "article", "fallback-55555"])
    }

    func testReplacingOuterLoaderPreservesActualKindRendererTable() {
        let customized = NostrContentRenderers.standard.eventLoader { input in
            NMPReferenceLiteral(original: input.occurrence.original)
        }

        XCTAssertTrue(customized.hasEventRenderer(kind: 1))
        XCTAssertTrue(customized.hasEventRenderer(kind: 30_023))
        XCTAssertFalse(customized.hasEventRenderer(kind: 55_555))
    }

    func testRenderContextStopsCyclesAndDepthWithoutMutableBudget() {
        let root = NostrContentRenderContext(
            ancestorTargetKeys: [],
            depth: 0,
            maximumDepth: 2
        )
        let first = root.descending(into: "event:a")
        XCTAssertEqual(first?.depth, 1)
        XCTAssertNil(first?.descending(into: "event:a"))
        let second = first?.descending(into: "event:b")
        XCTAssertEqual(second?.depth, 2)
        XCTAssertNil(second?.descending(into: "event:c"))
    }

    func testLiveFactoryAppearAndDisappearIsCleanWithoutNativeTaskCensus() throws {
        // #680 removed the native-task census and idle barrier: a live
        // observation no longer reserves a native task, so there is no census
        // to return to a baseline. The surviving invariant is that a
        // visible-reference observation appears, disappears, and re-appears
        // cleanly (the underlying async handle is opened and cancelled without
        // bricking the observation or the engine).
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        let observation = NMPVisibleReferenceObservation(
            target: .profile(pubkey: pubkey),
            factory: .live(engine: engine)
        )

        observation.appear()
        observation.disappear()
        observation.appear()
        observation.disappear()
    }

    func testAvatarFallbackColorIsStable() {
        let key = String(repeating: "01", count: 32)
        XCTAssertEqual(
            String(describing: NMPAvatar.placeholderColor(for: key)),
            String(describing: NMPAvatar.placeholderColor(for: key))
        )
    }

    func testArticlePresentationLeafTextUsesStableFallbacks() {
        let article = NMPArticlePresentation(
            author: "author",
            createdAt: 1,
            title: "   ",
            summary: "  useful summary  ",
            content: "one two three"
        )
        XCTAssertEqual(NMPArticleText.title(article), "Untitled article")
        XCTAssertEqual(NMPArticleText.summary(article), "useful summary")
    }

    func testAvatarGroupReportsOverflowDeterministically() {
        let people = (0..<6).map { index in
            NMPAvatarItem(pubkey: String(repeating: "\(index)", count: 64))
        }
        let group = NMPAvatarGroup(people: people, maximumVisible: 4)
        XCTAssertEqual(group.overflowCount, 2)
    }

    func testCompactCountsMatchReactionComponents() {
        XCTAssertEqual(NMPCompactCount.string(for: 999), "999")
        XCTAssertEqual(NMPCompactCount.string(for: 1_250), "1.2k")
    }

    func testFollowButtonBodyAcceptsOnlyProjectedStateAndATap() {
        let snapshot = NMPFollowingSnapshot(
            activePubkey: String(repeating: "01", count: 32),
            target: String(repeating: "02", count: 32),
            relationship: .following,
            availability: .ready,
            baseEventID: String(repeating: "03", count: 32)
        )

        _ = NMPFollowButtonBody(
            snapshot: snapshot,
            isActing: false,
            variant: .prominent,
            action: {}
        )
    }

    private func occurrence(
        target: NostrReferenceTarget,
        original: String
    ) -> NostrReferenceOccurrence {
        NostrReferenceOccurrence(
            id: 1,
            original: original,
            target: target,
            source: NostrContentSourceRange(start: 0, end: UInt32(original.utf8.count)),
            placement: .inline
        )
    }

    private func row(
        idByte: String,
        kind: UInt16,
        pubkey: String? = nil
    ) -> Row {
        Row(
            id: String(repeating: idByte, count: 32),
            pubkey: pubkey ?? self.pubkey,
            createdAt: 1,
            kind: kind,
            tags: [],
            content: "content",
            sig: String(repeating: "ef", count: 64),
            sources: []
        )
    }

    private func eventInput(
        occurrence: NostrReferenceOccurrence,
        row: Row,
        purpose: NostrContentPurpose,
        renderers: NostrContentRenderers
    ) -> NMPEventRenderInput {
        NMPEventRenderInput(
            occurrence: occurrence,
            event: row,
            purpose: purpose,
            context: .root,
            observationFactory: nil,
            renderers: renderers,
            actions: NostrContentActions()
        )
    }
}

private final class ObservationProbe: @unchecked Sendable {
    private struct Entry {
        var active: Bool
        let receive: NMPReferenceObservationFactory.Receive
    }

    private let lock = NSLock()
    private var entries: [Entry] = []
    private var cancellations = 0

    var factory: NMPReferenceObservationFactory {
        NMPReferenceObservationFactory { [self] _, receive in
            lock.lock()
            let index = entries.count
            entries.append(Entry(active: true, receive: receive))
            lock.unlock()
            return NMPReferenceObservationHandle { [self] in cancel(index) }
        }
    }

    var totalOpenCount: Int {
        lock.locked { entries.count }
    }

    var totalCancelCount: Int {
        lock.locked { cancellations }
    }

    var activeCount: Int {
        lock.locked { entries.filter(\.active).count }
    }

    @MainActor
    func send(_ batch: RowBatch, to index: Int) {
        let receive = lock.locked { entries[index].receive }
        receive(batch)
    }

    private func cancel(_ index: Int) {
        lock.locked {
            guard entries[index].active else { return }
            entries[index].active = false
            cancellations += 1
        }
    }
}

private final class RendererRecorder {
    private(set) var values: [String] = []

    func view(_ value: String) -> Text {
        values.append(value)
        return Text(value)
    }
}

private extension NSLock {
    func locked<T>(_ body: () -> T) -> T {
        lock()
        defer { unlock() }
        return body()
    }
}
