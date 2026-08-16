// Proves query descriptors are VALUES (M5 plan §3 table row 3): editing
// `model.kinds` here builds a brand-new `NMPFilter` the next time FeedView's
// `.task(id: model.kinds)` runs. No NMP call happens in this file at all --
// this screen touches only the app's own `@Observable` state.

import SwiftUI

struct KindsEditorView: View {
    let model: AppModel

    @State private var kindsText: String = "1"

    var body: some View {
        Form {
            Section("Edit kinds (comma-separated)") {
                TextField("e.g. 1,6", text: $kindsText)
                    .keyboardType(.numbersAndPunctuation)
                    .autocorrectionDisabled()
                Button("Apply") { apply() }
                    .disabled(parsed(kindsText).isEmpty)
            }

            Section("Currently active") {
                Text(model.kinds.map(String.init).joined(separator: ", "))
                    .font(.headline)
            }

            Section("Quick picks") {
                Button("kind:1 (notes)") { set([1]) }
                Button("kind:1 + kind:6 (notes + reposts)") { set([1, 6]) }
                Button("kind:7 (reactions)") { set([7]) }
            }
        }
        .navigationTitle("Kinds")
        .onAppear { kindsText = model.kinds.map(String.init).joined(separator: ",") }
    }

    private func apply() {
        let values = parsed(kindsText)
        if !values.isEmpty {
            model.kinds = values
        }
    }

    private func set(_ kinds: [UInt16]) {
        model.kinds = kinds
        kindsText = kinds.map(String.init).joined(separator: ",")
    }

    private func parsed(_ text: String) -> [UInt16] {
        text.split(separator: ",").compactMap { UInt16($0.trimmingCharacters(in: .whitespaces)) }
    }
}
