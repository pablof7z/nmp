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

import SwiftUI
import NMP

/// The app's own record of what one accepted write has reported so far.
/// `Receipt.status` is authoritative; this only retains facts already
/// delivered by that stream so the view has something to render between
/// deliveries -- never a value this app invented.
struct OutstandingWrite: Identifiable {
    let id: UInt64
    let preview: String
    var signing: SigningState?
    var destinations: (relays: [String], complete: Bool, awaitingAuthorRoutes: [String])?
    var relayStates: [String: RelayState] = [:]
    var outcome: WriteOutcome?
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

            if let signing = write.signing {
                LabeledContent("Signing", value: describe(signing))
            }
            if let destinations = write.destinations {
                LabeledContent(
                    "Destinations",
                    value: "\(destinations.relays.count) relay(s), complete: \(destinations.complete)"
                )
                if !destinations.awaitingAuthorRoutes.isEmpty {
                    LabeledContent(
                        "Awaiting routes for",
                        value: "\(destinations.awaitingAuthorRoutes.count) author(s)"
                    )
                }
            }
            ForEach(write.relayStates.keys.sorted(), id: \.self) { relay in
                if let state = write.relayStates[relay] {
                    LabeledContent(relay, value: describe(state))
                }
            }
            if let outcome = write.outcome {
                LabeledContent("Outcome", value: describe(outcome))
                    .font(.footnote.bold())
            }
        }
        .font(.caption)
        .padding(.vertical, 2)
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
        // No correlation token: recovering an outstanding write after a
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

    private func apply(_ fact: WriteFact, toWriteID id: UInt64) {
        guard let idx = writes.firstIndex(where: { $0.id == id }) else { return }
        switch fact {
        case .signing(let state):
            writes[idx].signing = state
        case .relay(_, let relay, let state):
            writes[idx].relayStates[relay] = state
        case .destinations(let relays, let complete, let awaitingAuthorRoutes):
            writes[idx].destinations = (relays, complete, awaitingAuthorRoutes)
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

    private func describe(_ state: RelayState) -> String {
        switch state {
        case .waiting(let waiting): return "waiting: \(describe(waiting))"
        case .sent(let attempt, _): return "sent (attempt \(attempt))"
        case .published: return "published"
        case .rejected(let reason): return "rejected: \(reason)"
        case .authFailed(_, let source, let reason): return "auth failed (\(source)): \(reason)"
        case .gaveUp: return "gave up"
        }
    }

    private func describe(_ waiting: RelayWaiting) -> String {
        switch waiting {
        case .notConnected: return "not connected"
        case .needsAuth: return "needs auth"
        case .backingOff(let attempt, _, let cause, let detail):
            return "backing off (attempt \(attempt), \(cause)\(detail.map { ": \($0)" } ?? ""))"
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
