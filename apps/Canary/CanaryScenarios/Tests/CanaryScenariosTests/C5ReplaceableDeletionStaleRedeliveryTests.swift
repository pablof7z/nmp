// C5 (docs/internals/canary.md "Scenario status"): replaceable events,
// deletion, and stale redelivery. A profile is superseded and the old
// version disappears; a kind:5 removes its target; and a relay that
// redelivers the superseded or the deleted event afterwards does NOT
// resurrect it.
//
// Run with `swift test` from apps/Canary/CanaryScenarios (see that
// package's README.md). macOS only, same reason as C1/C2/C7/C9/C13/C17.
// Public `NMP` API only: plain `import NMP`, no `@testable`, no internal
// crate, no direct Redb inspection. The three relays are reached only over
// real `ws://` URLs to three separate OS processes.
//
// THE THIRD PART IS THE ONE WORTH SEQUENCING, AND IT IS ALSO THE ONE THAT
// IS HARDEST TO PROVE HAPPENED AT ALL. A correct engine's response to a
// stale redelivery is to do NOTHING: no row change, no delivered batch, no
// app-visible fact of any kind. So "the row did not come back" is equally
// true of an engine that ignored the stale event and of a scenario where
// the stale event never arrived. Everything below is arranged so those two
// cannot be confused:
//
//   1. THE STALE RELAY STARTS EMPTY AND STAYS SUBSCRIBED. Relay B and
//      relay C are up, in the engine's relay set, and reporting a live wire
//      subscription from the beginning -- they simply have nothing. An
//      ordinary client REQ confirms each holds nothing for the id in
//      question before its phase.
//   2. THE STALE EVENT IS WRITTEN ONLY AFTER THE APP HAS ALREADY OBSERVED
//      THE SUPERSEDE / THE DELETION, asserted first, with a real EVENT
//      frame answered by a real OK -- so "after" is by construction, not by
//      hope.
//   3. A CONTROL EVENT FOLLOWS IT DOWN THE SAME PIPE. Immediately after the
//      stale write, a fresh event the app has never seen is written to the
//      SAME relay. The app's subscription to that relay is one TCP
//      connection and the relay pushes in ingest order, so the control
//      arriving is positive proof that the stale event was pushed to the
//      app first and silently refused. The control's own `sources` naming
//      that relay is the confirmation that this relay, specifically, is
//      what delivered it.
//
// Without (3) this scenario is exactly the vacuous shape C13's fourth
// falsifier and C17's liveness assertion exist to rule out, and the
// falsification section in docs/internals/canary.md records that removing
// the control makes a broken sequencing pass.
//
// A PRODUCT FACT DISCOVERED BY WRITING THIS: the feed's filter includes
// kind 5. It has to. Relays only send what an open subscription asked for,
// so an app that subscribes to kind 1 alone is never sent the deletion and
// its rows never go away -- NMP does not add kind:5 to an app's demand on
// the app's behalf. That is falsifier #2 below, and it is reported as an
// API finding rather than worked around.
//
// Every wait below is a bounded poll on a real condition with the real
// stuck values reported on timeout -- never a fixed sleep used AS the
// synchronization oracle.

import Foundation
import XCTest
import NMP
import RelayLabKit

final class C5ReplaceableDeletionStaleRedeliveryTests: XCTestCase {
    // MARK: - What one observation has been delivered

    private struct ObservedState: Sendable {
        var batches = 0
        var latest: [Row] = []
        var everSeen: Set<String> = []
        /// Ids this observation must never be delivered again from the
        /// moment `watch(forbidden:)` was called. Non-empty `resurrected`
        /// means a stale event came back.
        var forbidden: Set<String> = []
        var resurrected: Set<String> = []
        var duplicateWitness: String?
        var ended: String?

        func row(_ id: String) -> Row? { latest.first { $0.id == id } }
        var ids: Set<String> { Set(latest.map(\.id)) }
    }

    private actor ObservationLedger {
        private var state = ObservedState()

        func record(_ batch: RowBatch) {
            state.batches += 1
            let ids = batch.rows.map(\.id)
            if Set(ids).count != ids.count {
                state.duplicateWitness = ids.joined(separator: " ")
            }
            state.latest = batch.rows
            state.everSeen.formUnion(ids)
            state.resurrected.formUnion(state.forbidden.intersection(ids))
        }

        /// Start watching for ids that must never be delivered again. Called
        /// only once the app has been asserted to no longer hold them, so a
        /// hit is unambiguously a resurrection rather than a leftover.
        func watch(forbidden: Set<String>) {
            state.forbidden.formUnion(forbidden)
        }

        func markEnded(_ why: String) { state.ended = why }
        func current() -> ObservedState { state }
    }

    /// `observeDiagnostics()` is PUSH-only -- there is no synchronous "what
    /// is your current snapshot" call on `NMPEngine`, so an application that
    /// wants a point-in-time reading has to hold the stream open and keep
    /// the last value it was handed. Same nine lines as C13 and C17's
    /// churner, duplicated deliberately (`docs/internals/canary.md`: "a
    /// little duplication is preferable to hiding evidence").
    private final class LatestDiagnostics: @unchecked Sendable {
        private let lock = NSLock()
        private var value = DiagnosticsSnapshot()

        func store(_ snapshot: DiagnosticsSnapshot) {
            lock.lock()
            value = snapshot
            lock.unlock()
        }

        func current() -> DiagnosticsSnapshot {
            lock.lock()
            defer { lock.unlock() }
            return value
        }

        func subscribedRelays() -> Set<String> {
            Set(current().relays.filter { $0.wireSubCount >= 1 }.map(\.relay))
        }
    }

    @discardableResult
    private func waitUntil(
        timeout: TimeInterval = 30,
        _ condition: () async -> Bool
    ) async -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if await condition() { return true }
            try? await Task.sleep(nanoseconds: 25_000_000)
        }
        return await condition()
    }

    // MARK: - The scenario

    func testSupersedeDeletionAndAStaleRelayThatMustNotResurrectEither() async throws {
        let binaryPath = try Self.locateStrfryBinary()

        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("canary-c5-\(UUID().uuidString)")
        let storeDir = root.appendingPathComponent("store")
        try FileManager.default.createDirectory(at: storeDir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }

        // Three independent relay processes with three independent stores.
        // A is the app's ordinary relay. B and C are the stale ones: up,
        // subscribed and EMPTY from the start, so that when each is finally
        // written to, the write is genuinely a redelivery arriving after the
        // fact rather than a first delivery racing it.
        var relays: [RelayHandle] = []
        for name in ["c5-relay-a", "c5-stale-profile", "c5-stale-deleted"] {
            let dir = root.appendingPathComponent(name)
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            let handle = try await RelayHandle(name: name, workDir: dir, binaryPath: binaryPath)
            try await handle.start()
            relays.append(handle)
        }
        let relayA = relays[0], staleProfileRelay = relays[1], staleDeletedRelay = relays[2]

        let alice = try NostrKeyPair()
        let bob = try NostrKeyPair()
        let carol = try NostrKeyPair()

        // Timestamps are explicit throughout: replaceable supersession is
        // decided by `created_at`, and a scenario that lets the clock pick
        // them is not sequencing anything.
        let t0 = Int64(Date().timeIntervalSince1970) - 600

        // -- The replaceable subject: alice's profile, two versions.
        let profileV1 = try NostrSigning.sign(
            keyPair: alice, kind: 0, content: #"{"name":"alice","about":"C5 version one"}"#,
            createdAt: t0
        )
        let profileV2 = try NostrSigning.sign(
            keyPair: alice, kind: 0, content: #"{"name":"alice","about":"C5 version two"}"#,
            createdAt: t0 + 60
        )
        // The control for the profile phase: a profile the app has never
        // seen, written to the stale relay immediately after the stale one.
        let bobProfile = try NostrSigning.sign(
            keyPair: bob, kind: 0, content: #"{"name":"bob","about":"C5 profile control"}"#,
            createdAt: t0 + 120
        )

        // -- The deletion subject: two of alice's notes, one of them deleted.
        let deletionTarget = try NostrSigning.sign(
            keyPair: alice, kind: 1, content: "C5 the note alice deletes", createdAt: t0 + 10
        )
        let survivor = try NostrSigning.sign(
            keyPair: alice, kind: 1, content: "C5 the note alice keeps", createdAt: t0 + 20
        )
        let deletion = try NostrSigning.sign(
            keyPair: alice, kind: 5, tags: [["e", deletionTarget.id]],
            content: "C5 deleting one note", createdAt: t0 + 90
        )
        // The control for the deletion phase.
        let carolNote = try NostrSigning.sign(
            keyPair: carol, kind: 1, content: "C5 note control", createdAt: t0 + 130
        )

        // Seeded before the engine exists: an already-populated relay and an
        // empty local store, the same cold start C1 uses.
        try await relayA.seed(profileV1)
        try await relayA.seed(deletionTarget)
        try await relayA.seed(survivor)

        let engine = try NMPEngine(
            config: NMPConfig(
                storePath: storeDir.appendingPathComponent("nmp.redb").path,
                appRelays: [relayA.url, staleProfileRelay.url, staleDeletedRelay.url]
            )
        )
        defer { engine.shutdown() }

        let diagnostics = LatestDiagnostics()
        let diagnosticsStream = try engine.observeDiagnostics()
        let diagnosticsPump = Task {
            do {
                for try await snapshot in diagnosticsStream { diagnostics.store(snapshot) }
            } catch {}
        }
        defer {
            diagnosticsPump.cancel()
            diagnosticsStream.cancel()
        }

        // TWO observations, both open for the whole scenario, neither ever
        // reopened: a reopened query reads the settled store and would prove
        // nothing about what a live feed did when the supersede or the
        // deletion landed.
        //
        // KIND 5 IS IN THE FEED'S FILTER ON PURPOSE. See the header: a relay
        // sends only what a subscription asked for, so without it the
        // deletion never reaches the app at all. It is not a workaround for
        // an NMP defect -- it is what an app must actually declare -- but it
        // is a thing an app author has to know, and it is recorded as a
        // finding.
        let profiles = try engine.observe(
            .single(NMPDemand(selection: NMPFilter(kinds: [0], authors: .literal([alice.pubkeyHex, bob.pubkeyHex]))))
        )
        let feed = try engine.observe(
            .single(NMPDemand(selection: NMPFilter(kinds: [1, 5], authors: .literal([alice.pubkeyHex, carol.pubkeyHex]))))
        )

        let profileLedger = ObservationLedger()
        let feedLedger = ObservationLedger()
        let consumers = Task {
            await withTaskGroup(of: Void.self) { group in
                group.addTask {
                    do {
                        for try await batch in profiles { await profileLedger.record(batch) }
                        await profileLedger.markEnded("sequence ended")
                    } catch {
                        await profileLedger.markEnded("threw: \(error)")
                    }
                }
                group.addTask {
                    do {
                        for try await batch in feed { await feedLedger.record(batch) }
                        await feedLedger.markEnded("sequence ended")
                    } catch {
                        await feedLedger.markEnded("threw: \(error)")
                    }
                }
            }
        }
        defer { consumers.cancel() }

        var log: [String] = []
        func note(_ line: String) { log.append(line) }
        defer { print((["", "C5 phase log:"] + log).joined(separator: "\n")) }

        // --- Phase 0: everything is genuinely live ------------------------
        //
        // All three relays subscribed, and the app holding the pre-supersede,
        // pre-deletion state. Both stale relays must already be on the wire
        // here: a relay the engine only subscribes to later would turn the
        // redeliveries below into first deliveries.

        let sawV1 = await waitUntil(timeout: 30) {
            await profileLedger.current().ids == [profileV1.id]
        }
        let sawBothNotes = await waitUntil(timeout: 30) {
            await feedLedger.current().ids == [deletionTarget.id, survivor.id]
        }
        let allSubscribed = await waitUntil(timeout: 30) {
            diagnostics.subscribedRelays().isSuperset(
                of: [relayA.url, staleProfileRelay.url, staleDeletedRelay.url]
            )
        }
        let start0 = await profileLedger.current()
        let startFeed = await feedLedger.current()
        note(
            "start: sawV1=\(sawV1) sawBothNotes=\(sawBothNotes) allSubscribed=\(allSubscribed) "
                + "subscribed=\(diagnostics.subscribedRelays().count)/3 "
                + "profiles=\(start0.ids.count) rows/\(start0.batches) batches "
                + "feed=\(startFeed.ids.count) rows/\(startFeed.batches) batches"
        )
        XCTAssertTrue(
            sawV1,
            "PRECONDITION: the app must hold exactly profile v1 (\(profileV1.id)) before it is "
                + "superseded -- it holds \(start0.ids) after \(start0.batches) batch(es), ended "
                + "\(start0.ended ?? "no"). With no old version present, 'the old version "
                + "disappeared' is not a claim about anything."
        )
        XCTAssertTrue(
            sawBothNotes,
            "PRECONDITION: the app must hold both of alice's notes before one is deleted -- it "
                + "holds \(startFeed.ids) after \(startFeed.batches) batch(es), ended "
                + "\(startFeed.ended ?? "no")"
        )
        XCTAssertTrue(
            allSubscribed,
            "PRECONDITION: only \(diagnostics.subscribedRelays().count) of the 3 relays report a "
                + "wire subscription (\(diagnostics.subscribedRelays().sorted())). The two stale "
                + "relays must ALREADY be on the wire, or writing to them later is a first "
                + "delivery rather than a redelivery."
        )

        // --- Phase 1: the replaceable is superseded -----------------------

        let v2Accepted = try await relayA.seed(profileV2)
        XCTAssertTrue(v2Accepted, "relay A refused profile v2")
        let superseded = await waitUntil(timeout: 30) {
            await profileLedger.current().ids == [profileV2.id]
        }
        let afterSupersede = await profileLedger.current()
        note(
            "supersede: superseded=\(superseded) rows=\(afterSupersede.latest.count) "
                + "ids=\(afterSupersede.ids) batches=\(afterSupersede.batches) "
                + "content=\(afterSupersede.row(profileV2.id)?.content ?? "none")"
        )
        XCTAssertTrue(
            superseded,
            "the superseding profile did not replace the old one: the app holds "
                + "\(afterSupersede.latest.count) row(s) \(afterSupersede.ids) 30s after v2 "
                + "(\(profileV2.id), created_at \(profileV2.created_at)) was accepted by the relay, "
                + "against v1 \(profileV1.id) at \(profileV1.created_at). Two rows means the old "
                + "version is still on screen; one wrong row means the wrong version won."
        )
        XCTAssertEqual(
            afterSupersede.row(profileV2.id)?.content, profileV2.content,
            "the surviving profile row must carry v2's content, not v1's text under v2's id"
        )
        XCTAssertFalse(
            afterSupersede.ids.contains(profileV1.id),
            "the superseded profile \(profileV1.id) is still in the delivered set"
        )
        await profileLedger.watch(forbidden: [profileV1.id])

        // --- Phase 2: a relay redelivers the SUPERSEDED version -----------
        //
        // The stale relay has been subscribed since phase 0 and is empty of
        // this id (confirmed by an ordinary client REQ, a fact about the
        // FIXTURE rather than a reading of NMP's state). v1 goes in first,
        // then the control. Both over real EVENT frames with real OKs.

        let staleHadItBefore = try await staleProfileRelay.queryById(profileV1.id) != nil
        XCTAssertFalse(
            staleHadItBefore,
            "PRECONDITION: \(staleProfileRelay.url) already held \(profileV1.id) before this "
                + "phase, so it may have delivered it before the supersede rather than after"
        )
        let staleProfileAccepted = try await staleProfileRelay.seed(profileV1)
        let controlProfileAccepted = try await staleProfileRelay.seed(bobProfile)

        // The control is the whole precondition: if it arrives, this relay's
        // push path to the app was live at this instant, and the stale event
        // -- written to the same relay, over the same subscription, first --
        // was pushed too.
        let controlArrived = await waitUntil(timeout: 45) {
            await profileLedger.current().ids.contains(bobProfile.id)
        }
        // A settle beat AFTER the control, so a slow resurrection has the
        // same time to appear that the control had.
        try? await Task.sleep(nanoseconds: 1_500_000_000)
        let afterStaleProfile = await profileLedger.current()
        note(
            "stale profile: accepted=\(staleProfileAccepted) controlAccepted="
                + "\(controlProfileAccepted) controlArrived=\(controlArrived) "
                + "controlSources=\(afterStaleProfile.row(bobProfile.id)?.sources ?? []) "
                + "ids=\(afterStaleProfile.ids) resurrected=\(afterStaleProfile.resurrected) "
                + "batches=\(afterStaleProfile.batches)"
        )
        XCTAssertTrue(staleProfileAccepted, "the stale relay refused the superseded profile v1")
        XCTAssertTrue(controlProfileAccepted, "the stale relay refused the control profile")
        XCTAssertTrue(
            controlArrived,
            "PRECONDITION: the control profile \(bobProfile.id), written to "
                + "\(staleProfileRelay.url) immediately AFTER the stale v1, never reached the app "
                + "(holds \(afterStaleProfile.ids), \(afterStaleProfile.batches) batches, ended "
                + "\(afterStaleProfile.ended ?? "no")). Without it there is no evidence the stale "
                + "event was delivered at all, and 'it did not resurrect' would be vacuous."
        )
        XCTAssertEqual(
            afterStaleProfile.row(bobProfile.id)?.sources ?? [], [staleProfileRelay.url],
            "PRECONDITION: the control must be credited to \(staleProfileRelay.url) specifically "
                + "-- that is what identifies which relay's push path was proven live"
        )
        XCTAssertTrue(
            afterStaleProfile.resurrected.isEmpty,
            "STALE REDELIVERY RESURRECTED A SUPERSEDED EVENT: \(afterStaleProfile.resurrected) was "
                + "delivered again after the app had already moved to v2. The app now shows an "
                + "outdated profile because one relay happened to be behind."
        )
        XCTAssertEqual(
            afterStaleProfile.ids, [profileV2.id, bobProfile.id],
            "after the stale redelivery the profile set must be exactly v2 and the control"
        )
        XCTAssertEqual(
            afterStaleProfile.row(profileV2.id)?.content, profileV2.content,
            "v2's content changed after the stale redelivery -- v1's payload reached the row "
                + "even though its id did not"
        )
        XCTAssertFalse(
            afterStaleProfile.row(profileV2.id)?.sources.contains(staleProfileRelay.url) ?? true,
            "v2 is credited to \(staleProfileRelay.url), which only ever supplied v1. `Row.sources` "
                + "is the set of relays that delivered THIS event id; crediting a relay for an "
                + "event it never sent makes the field unusable. Got "
                + "\(afterStaleProfile.row(profileV2.id)?.sources ?? [])"
        )

        // --- Phase 3: a kind:5 deletion removes its target ----------------

        let deletionAccepted = try await relayA.seed(deletion)
        XCTAssertTrue(deletionAccepted, "relay A refused the deletion event")
        let deleted = await waitUntil(timeout: 30) {
            await !feedLedger.current().ids.contains(deletionTarget.id)
        }
        let afterDeletion = await feedLedger.current()
        note(
            "deletion: deleted=\(deleted) ids=\(afterDeletion.ids) "
                + "rows=\(afterDeletion.latest.count) batches=\(afterDeletion.batches)"
        )
        XCTAssertTrue(
            deleted,
            "the kind:5 deletion \(deletion.id) did not remove its target \(deletionTarget.id): "
                + "the feed still holds \(afterDeletion.ids) after 30s "
                + "(\(afterDeletion.batches) batches, ended \(afterDeletion.ended ?? "no"))"
        )
        XCTAssertTrue(
            afterDeletion.ids.contains(survivor.id),
            "the deletion removed more than its target -- the untargeted note \(survivor.id) is "
                + "gone too. Delivered: \(afterDeletion.ids)"
        )
        // The deletion event itself matches this filter, so an app that asks
        // for kind 5 gets a row for it. NMP does no display filtering (raw
        // tokens only) -- recorded here so the expected set below is not
        // mistaken for a defect.
        XCTAssertEqual(
            afterDeletion.ids, [survivor.id, deletion.id],
            "after the deletion the feed must be exactly the surviving note plus the kind:5 event "
                + "the app itself subscribed to"
        )
        await feedLedger.watch(forbidden: [deletionTarget.id])

        // --- Phase 4: a relay redelivers the DELETED event ----------------
        //
        // Same shape as phase 2, against the second stale relay, which has
        // never been told anything was deleted.

        let staleDeletedHadItBefore = try await staleDeletedRelay.queryById(deletionTarget.id) != nil
        XCTAssertFalse(
            staleDeletedHadItBefore,
            "PRECONDITION: \(staleDeletedRelay.url) already held \(deletionTarget.id) before this "
                + "phase, so it may have delivered it before the deletion rather than after"
        )
        let staleDeletedAccepted = try await staleDeletedRelay.seed(deletionTarget)
        let controlNoteAccepted = try await staleDeletedRelay.seed(carolNote)

        let controlNoteArrived = await waitUntil(timeout: 45) {
            await feedLedger.current().ids.contains(carolNote.id)
        }
        try? await Task.sleep(nanoseconds: 1_500_000_000)
        let afterStaleDeleted = await feedLedger.current()
        note(
            "stale deleted: accepted=\(staleDeletedAccepted) controlAccepted=\(controlNoteAccepted) "
                + "controlArrived=\(controlNoteArrived) "
                + "controlSources=\(afterStaleDeleted.row(carolNote.id)?.sources ?? []) "
                + "ids=\(afterStaleDeleted.ids) resurrected=\(afterStaleDeleted.resurrected) "
                + "batches=\(afterStaleDeleted.batches)"
        )
        XCTAssertTrue(staleDeletedAccepted, "the stale relay refused the deleted event")
        XCTAssertTrue(controlNoteAccepted, "the stale relay refused the control note")
        XCTAssertTrue(
            controlNoteArrived,
            "PRECONDITION: the control note \(carolNote.id), written to \(staleDeletedRelay.url) "
                + "immediately AFTER the deleted event, never reached the app (holds "
                + "\(afterStaleDeleted.ids), \(afterStaleDeleted.batches) batches, ended "
                + "\(afterStaleDeleted.ended ?? "no")). Without it there is no evidence the "
                + "deleted event was redelivered at all."
        )
        XCTAssertEqual(
            afterStaleDeleted.row(carolNote.id)?.sources ?? [], [staleDeletedRelay.url],
            "PRECONDITION: the control must be credited to \(staleDeletedRelay.url) specifically"
        )
        XCTAssertTrue(
            afterStaleDeleted.resurrected.isEmpty,
            "STALE REDELIVERY RESURRECTED A DELETED EVENT: \(afterStaleDeleted.resurrected) came "
                + "back after alice's kind:5 removed it. A deletion that one out-of-date relay can "
                + "undo is not a deletion."
        )
        XCTAssertEqual(
            afterStaleDeleted.ids, [survivor.id, deletion.id, carolNote.id],
            "after the stale redelivery the feed must be exactly the survivor, the deletion event, "
                + "and the control"
        )
        XCTAssertNil(
            afterStaleDeleted.duplicateWitness,
            "a batch carried the same id twice: \(afterStaleDeleted.duplicateWitness ?? "")"
        )
        XCTAssertNil(
            afterStaleProfile.duplicateWitness,
            "a profile batch carried the same id twice: \(afterStaleProfile.duplicateWitness ?? "")"
        )

        consumers.cancel()
        profiles.cancel()
        feed.cancel()
        for relay in relays { try await relay.kill() }
    }

    /// `setup-strfry.sh`'s own default cache location. Same shape as every
    /// other scenario in this package: skip by name rather than crash.
    private static func locateStrfryBinary() throws -> URL {
        let cacheDir = ProcessInfo.processInfo.environment["RELAY_LAB_CACHE_DIR"]
            ?? (NSHomeDirectory() + "/Library/Caches/nmp-canary-relay-lab")
        let binary = URL(fileURLWithPath: cacheDir).appendingPathComponent("strfry/strfry")
        guard FileManager.default.isExecutableFile(atPath: binary.path) else {
            throw XCTSkip(
                "strfry is not built at \(binary.path) -- run apps/Canary/setup-strfry.sh first"
            )
        }
        return binary
    }
}
