// The app's first screen that ever calls `engine.publish` (C5/C7/C8/C9/C10/
// C12). The only NMP calls here are `engine.publish(_:)`, iterating
// `Receipt.status`, and, via `AppModel` at launch, `engine.publishQueue`
// and `engine.reattachReceipt(id:)`. Everything else -- the per-relay
// rendering -- is this app's own, exactly like `FeedView`/`DiagnosticsView`.
// There is no app-owned ledger of outstanding writes here: NMP's own
// publish queue is that ledger (#1770).
//
// The one rule this screen exists to prove or disprove: it must never poll,
// retry, or chase a relay itself. If truthfully rendering what happened to a
// write ever seemed to require that, that would be an NMP API finding, not
// something to route around here.
//
// It answers three questions about a publish, in the order a person asks
// them: WHAT did I publish (the event id), WHERE was it meant to go (the
// intended relay set, and whether routing has finished choosing it), and WHAT
// HAPPENED at each of those relays -- including, verbatim, what the relay
// itself said.
//
// Two of those are answered only partly, and both gaps are rendered rather
// than hidden. The event id is not returned by `publish`; it appears when a
// fact first quotes it. And the relay's message on a SUCCESSFUL publish does
// not exist in NMP at all -- see `relayMessageRow`. Showing a gap is this
// screen's job as much as showing a success is; an app that silently rendered
// only the failure messages would leave the impression that the API carries
// both.

import SwiftUI
import NMP

/// The relays one write is INTENDED for, exactly as `WriteFact.destinations`
/// delivered them. `complete` is kept alongside the set because the set alone
/// cannot be read: while it is `false` the set can still GROW, so it is not a
/// denominator yet and nothing here may be rendered as "n of m".
struct WriteDestinations {
    var relays: [String]
    var complete: Bool
    var awaitingAuthorRoutes: [String]
}

/// The app's own record of what one accepted write has reported so far.
/// `Receipt.status` is authoritative; this only retains facts already
/// delivered by that stream so the view has something to render between
/// deliveries -- never a value this app invented.
struct OutstandingWrite: Identifiable {
    /// The store-issued RECEIPT id (`Receipt.id`) -- not the event id.
    let id: UInt64
    let preview: String
    /// The frozen event id, `nil` until the stream reveals it. `Receipt`
    /// carries only the receipt id, so this app learns the event id from the
    /// facts that quote it -- see `apply(_:toWriteID:)`.
    var eventID: String?
    var signing: SigningState?
    var destinations: WriteDestinations?
    var relayStates: [String: RelayState] = [:]
    var outcome: WriteOutcome?

    /// Every relay this write is known to involve: the intended set, plus any
    /// relay that has already reported. They are usually the same set, but a
    /// relay fact can arrive before the `destinations` fact that names it, and
    /// neither may be dropped from the picture.
    var knownRelays: [String] {
        Set((destinations?.relays ?? []) + relayStates.keys).sorted()
    }

    /// Only meaningful once `destinations.complete` is true -- before that
    /// there is no denominator for this to be a fraction of.
    var publishedCount: Int {
        relayStates.values.filter { $0 == .published }.count
    }
}

struct ComposeView: View {
    let model: AppModel

    @State private var content: String = ""
    @State private var explicitIdentityID: Data?
    @State private var writes: [OutstandingWrite] = []
    @State private var isPublishing = false

    var body: some View {
        NavigationStack {
            Form {
                Section("New note (kind:1)") {
                    TextField("What's on your mind?", text: $content, axis: .vertical)
                        .lineLimit(3...8)

                    // Identity override (C12): a write can be pinned to a
                    // SPECIFIC account regardless of which one is active when
                    // it is eventually delivered -- `Identity.explicit`, not
                    // `.active`.
                    Picker("Publish as", selection: $explicitIdentityID) {
                        Text("Active account").tag(Data?.none)
                        ForEach(model.accounts) { account in
                            Text(account.label).tag(Optional(account.id))
                        }
                    }

                    Button {
                        publish()
                    } label: {
                        if isPublishing {
                            ProgressView()
                        } else {
                            Text("Publish")
                        }
                    }
                    .disabled(content.isEmpty || isPublishing)
                }

                if let error = model.lastError {
                    Section("Last error") {
                        Text(error).foregroundStyle(.red).font(.caption)
                    }
                }

                Section("Outstanding writes") {
                    if writes.isEmpty {
                        Text("Nothing published yet.")
                            .foregroundStyle(.secondary)
                    }
                    ForEach(writes) { write in
                        writeSection(write)
                    }
                }
            }
            .navigationTitle("Compose")
            .task {
                // Crash-during-publication recovery (C9): whatever `AppModel`
                // found still open in NMP's own publish queue gets the same
                // live observation a fresh publish gets, below.
                for reattached in model.takeReattachedWrites() {
                    observe(reattached.receipt, preview: "(reattached after restart)")
                }
            }
        }
    }

    @ViewBuilder
    private func writeSection(_ write: OutstandingWrite) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(write.preview).font(.body)

            // The event id, which is what a person actually needs to go and
            // look this note up anywhere else. `Receipt` hands back only the
            // receipt id, so until a fact quotes the event id there is nothing
            // truthful to print here -- say that rather than print a blank.
            //
            // In full, and selectable: an id abbreviated to fit is an id
            // nobody can paste into a relay query, which is the entire reason
            // to show one.
            if let eventID = write.eventID {
                VStack(alignment: .leading, spacing: 0) {
                    Text("Event id").foregroundStyle(.secondary)
                    Text(eventID)
                        .font(.caption2.monospaced())
                        .textSelection(.enabled)
                }
            } else {
                LabeledContent("Event id", value: "not reported yet")
                    .foregroundStyle(.secondary)
            }

            if let signing = write.signing {
                LabeledContent("Signing", value: describe(signing))
            }

            destinationsRows(write)
            relayRows(write)

            if let outcome = write.outcome {
                LabeledContent("Outcome", value: describe(outcome))
                    .font(.footnote.bold())
            }
        }
        .font(.caption)
        .padding(.vertical, 2)
    }

    /// Where routing decided this write goes -- and, just as load-bearing,
    /// whether it has finished deciding. An incomplete set may still GROW, so
    /// it is never rendered as a total and never gets a "n of m" beside it.
    @ViewBuilder
    private func destinationsRows(_ write: OutstandingWrite) -> some View {
        if let destinations = write.destinations {
            if destinations.complete {
                LabeledContent(
                    "Routing",
                    value: "decided: \(destinations.relays.count) relay(s)"
                )
                // Only now is there a denominator: the destination set is
                // closed, so this fraction is a fact rather than a guess.
                LabeledContent(
                    "Published",
                    value: "\(write.publishedCount) of \(destinations.relays.count)"
                )
            } else if destinations.relays.isEmpty {
                LabeledContent("Routing", value: "still deciding -- no relay named yet")
                    .foregroundStyle(.orange)
            } else {
                LabeledContent(
                    "Routing",
                    value: "still deciding -- \(destinations.relays.count) so far, may grow"
                )
                .foregroundStyle(.orange)
            }

            // The keys resolution is still waiting on ARE the repair list, so
            // they are shown as keys, not counted into a number nobody can act
            // on.
            ForEach(destinations.awaitingAuthorRoutes, id: \.self) { pubkey in
                LabeledContent("Awaiting routes for", value: shortHex(pubkey))
                    .foregroundStyle(.orange)
            }
        } else {
            LabeledContent("Routing", value: "no destinations fact yet")
                .foregroundStyle(.secondary)
        }
    }

    /// One block per relay: what happened there, and what the relay itself
    /// said about it.
    @ViewBuilder
    private func relayRows(_ write: OutstandingWrite) -> some View {
        ForEach(write.knownRelays, id: \.self) { relay in
            VStack(alignment: .leading, spacing: 2) {
                if let state = write.relayStates[relay] {
                    LabeledContent(relay, value: describe(state))
                    relayMessageRow(state)
                } else {
                    LabeledContent(relay, value: "intended, nothing reported yet")
                        .foregroundStyle(.secondary)
                }
            }
        }
    }

    /// What the relay said, VERBATIM, wherever NMP carries it.
    ///
    /// It carries the message for every answer except the one an app most
    /// wants to show. `RelayState.published` is a case with no payload:
    /// `handle_write_ack` classifies `["OK", id, true, message]` as an ack and
    /// drops `message` on the floor, so there is no empty string to render and
    /// no "" to mistake for one -- the text simply does not exist anywhere in
    /// NMP. The same happens to a relay's `duplicate: ...` explanation, which
    /// is classified as an ack too.
    ///
    /// This app therefore says so out loud instead of quietly rendering only
    /// the failure cases. Proving what an app CANNOT get through the public
    /// API is as much this screen's job as proving what it can.
    @ViewBuilder
    private func relayMessageRow(_ state: RelayState) -> some View {
        switch state {
        case .published:
            LabeledContent("Relay said", value: "unavailable -- NMP discards the OK message")
                .foregroundStyle(.orange)
                .italic()
        case .rejected(let reason):
            LabeledContent("Relay said", value: reason)
                .textSelection(.enabled)
        case .authFailed(_, let source, let reason):
            // Only the `.relay` source is the relay talking; the other two are
            // this device's own refusal and must never be shown as a relay
            // rejecting the user.
            LabeledContent(source == .relay ? "Relay said" : "Refused locally", value: reason)
                .textSelection(.enabled)
        case .waiting(.backingOff(_, _, _, let detail)):
            if let detail {
                LabeledContent("Relay said", value: detail)
                    .textSelection(.enabled)
            }
        case .waiting, .sent, .gaveUp:
            // `.gaveUp` carries nothing: the ceiling was reached, and whatever
            // each failed attempt's relay message was is not retained on the
            // terminal state either.
            EmptyView()
        }
    }

    private func publish() {
        guard !content.isEmpty else { return }

        let identity: Identity = explicitIdentityID.map { .explicit(pubkey: hexString($0)) } ?? .active
        // `.auto` outbox routing is refused outright: `AppModel`'s engine has
        // no `NMPConfig.outboxRouting` indexer configured (that capability is
        // separate from the `.authorOutboxes` READ demand `FeedFilters`
        // uses, which needs no such config) -- see the report for why this
        // is worth naming as an API finding rather than quietly working
        // around. `.explicit` to the same two operator relays the feed
        // already trusts is the honest choice available today.
        //
        // No app-owned label: recovering an outstanding write after a
        // restart goes through NMP's own `publishQueue` (see `AppModel`),
        // not an app-remembered label, so nothing here needs to mint or
        // persist one (#1770).
        let intent = WriteIntent(
            payload: .event(kind: 1, content: content),
            routing: .explicit(relays: AppModel.appRelays),
            identity: identity
        )

        let preview = content
        content = ""
        isPublishing = true

        Task {
            defer { isPublishing = false }
            do {
                let receipt = try await model.engine.publish(intent)
                observe(receipt, preview: preview)
            } catch {
                model.lastError = "\(error)"
            }
        }
    }

    /// The one live-observation loop, shared by a fresh publish and a
    /// reattached one. It never asks for anything again -- it renders
    /// exactly what `receipt.status` delivers, in order, until the stream
    /// ends.
    private func observe(_ receipt: Receipt, preview: String) {
        writes.insert(OutstandingWrite(id: receipt.id, preview: preview), at: 0)
        Task {
            do {
                for try await fact in receipt.status {
                    apply(fact, toWriteID: receipt.id)
                }
            } catch {
                model.lastError = "\(error)"
            }
        }
    }

    /// Folds one delivered fact into this app's record. The event id is
    /// harvested from the two facts that quote it -- `.signing(.signed)` and
    /// `.relay` -- because `publish` does not return it: `Receipt` exposes
    /// `id`, the store-issued RECEIPT id used for reattach and cancel, and
    /// nothing else. So the id of the thing actually published only becomes
    /// showable once a fact mentions it.
    private func apply(_ fact: WriteFact, toWriteID id: UInt64) {
        guard let idx = writes.firstIndex(where: { $0.id == id }) else { return }
        switch fact {
        case .signing(let state):
            writes[idx].signing = state
            if case .signed(let eventID) = state {
                writes[idx].eventID = eventID
            }
        case .relay(let eventID, let relay, let state):
            writes[idx].eventID = eventID
            writes[idx].relayStates[relay] = state
        case .destinations(let relays, let complete, let awaitingAuthorRoutes):
            writes[idx].destinations = WriteDestinations(
                relays: relays,
                complete: complete,
                awaitingAuthorRoutes: awaitingAuthorRoutes
            )
        case .outcome(let outcome):
            writes[idx].outcome = outcome
        }
    }

    private func hexString(_ data: Data) -> String {
        data.map { String(format: "%02x", $0) }.joined()
    }

    private func describe(_ state: SigningState) -> String {
        switch state {
        case .awaitingSigner(let pubkey): return "awaiting signer (\(shortHex(pubkey)))"
        case .inFlight(let pubkey): return "in flight (\(shortHex(pubkey)))"
        case .signed(let eventId): return "signed (\(shortHex(eventId)))"
        case .refused(let reason): return "refused: \(reason)"
        }
    }

    /// The STATE only. Anything the relay itself said is rendered separately
    /// and verbatim by `relayMessageRow`, so it is deliberately not folded
    /// into this sentence -- a quoted relay message must appear exactly once,
    /// unedited, and be selectable.
    private func describe(_ state: RelayState) -> String {
        switch state {
        case .waiting(let waiting): return "waiting: \(describe(waiting))"
        case .sent(let attempt, _): return "sent (attempt \(attempt))"
        case .published: return "published"
        case .rejected: return "rejected"
        case .authFailed(_, let source, _): return "auth failed (\(source))"
        case .gaveUp: return "gave up"
        }
    }

    private func describe(_ waiting: RelayWaiting) -> String {
        switch waiting {
        case .notConnected: return "not connected"
        case .needsAuth: return "needs auth"
        case .backingOff(let attempt, _, let cause, _):
            return "backing off (attempt \(attempt), \(cause))"
        // Local disk, not a relay -- never quoted as something a relay said.
        case .persistenceStalled(let detail): return "persistence stalled: \(detail)"
        }
    }

    private func describe(_ outcome: WriteOutcome) -> String {
        switch outcome {
        case .settled: return "settled"
        case .noDestination: return "no destination"
        case .notSent(let reason): return "not sent: \(reason)"
        case .superseded: return "superseded"
        case .refused(let reason): return "refused: \(reason)"
        }
    }

    private func shortHex(_ hex: String) -> String {
        guard hex.count > 16 else { return hex }
        return "\(hex.prefix(8))…\(hex.suffix(8))"
    }
}
