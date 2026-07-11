// Multi-nsec login + runtime account switch (M5 plan §3 table row 1).
// Proves SignerRegistry + identity-as-input: `engine.addAccount` /
// `engine.setActiveAccount` are the only two NMP calls this screen makes.
// Everything else -- the account list, labels, which row is "active" -- is
// this app's OWN state (`AppModel.accounts`), not anything NMP tracks.

import SwiftUI

struct AccountsView: View {
    let model: AppModel

    @State private var newLabel: String = ""
    @State private var secretKeyInput: String = ""
    @State private var readOnlyLabel: String = "fiatjaf (read-only)"
    @State private var readOnlyHex: String =
        "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d"
    @State private var isAdding = false

    var body: some View {
        NavigationStack {
            Form {
                Section("Accounts") {
                    if model.accounts.isEmpty {
                        Text("No accounts yet -- add one below.")
                            .foregroundStyle(.secondary)
                    }
                    ForEach(model.accounts) { account in
                        Button {
                            model.setActive(account.id)
                        } label: {
                            HStack {
                                VStack(alignment: .leading, spacing: 2) {
                                    Text(account.label)
                                        .foregroundStyle(.primary)
                                    Text(shortHex(account.id))
                                        .font(.caption)
                                        .foregroundStyle(.secondary)
                                }
                                Spacer()
                                if account.kind == .readOnly {
                                    Text("read-only")
                                        .font(.caption2)
                                        .foregroundStyle(.secondary)
                                }
                                if model.activePubkey == account.id {
                                    Image(systemName: "checkmark.circle.fill")
                                        .foregroundStyle(.green)
                                }
                            }
                        }
                    }
                }

                Section("Add account (nsec or hex secret key)") {
                    TextField("Label", text: $newLabel)
                    SecureField("nsec1… or 64-char hex", text: $secretKeyInput)
                    Button {
                        isAdding = true
                        Task {
                            await model.addKeyedAccount(
                                secretKey: secretKeyInput,
                                label: newLabel.isEmpty ? "account" : newLabel
                            )
                            isAdding = false
                            secretKeyInput = ""
                        }
                    } label: {
                        if isAdding {
                            ProgressView()
                        } else {
                            Text("Add + activate")
                        }
                    }
                    .disabled(secretKeyInput.isEmpty || isAdding)
                }

                Section("Add read-only (pubkey only, no key)") {
                    TextField("Label", text: $readOnlyLabel)
                    TextField("Pubkey hex", text: $readOnlyHex)
                        .autocorrectionDisabled()
                    Button("Add + activate") {
                        model.addReadOnlyAccount(pubkeyHex: readOnlyHex, label: readOnlyLabel)
                    }
                    .disabled(readOnlyHex.isEmpty)
                }

                if let error = model.lastError {
                    Section("Last error") {
                        Text(error).foregroundStyle(.red).font(.caption)
                    }
                }
            }
            .navigationTitle("Accounts")
        }
    }

    private func shortHex(_ hex: String) -> String {
        guard hex.count > 16 else { return hex }
        return "\(hex.prefix(8))…\(hex.suffix(8))"
    }
}
