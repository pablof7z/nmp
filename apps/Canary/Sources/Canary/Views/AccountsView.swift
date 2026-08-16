// Whole-session account generation, public-key-only browsing, and switching.
// NMP owns account membership/current selection; labels and presentation stay
// in the app's own model.

import SwiftUI

struct AccountsView: View {
    let model: AppModel

    @State private var newLabel: String = ""
    @State private var publicKeyOnlyLabel: String = "fiatjaf (public key only)"
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
                            model.makeCurrent(account)
                        } label: {
                            HStack {
                                VStack(alignment: .leading, spacing: 2) {
                                    Text(account.label)
                                        .foregroundStyle(.primary)
                                    Text(shortKey(account.id))
                                        .font(.caption)
                                        .foregroundStyle(.secondary)
                                }
                                Spacer()
                                if account.kind == .publicKeyOnly {
                                    Text("public key only")
                                        .font(.caption2)
                                        .foregroundStyle(.secondary)
                                }
                                if model.currentPubkey == account.id {
                                    Image(systemName: "checkmark.circle.fill")
                                        .foregroundStyle(.green)
                                }
                            }
                        }
                    }
                }

                Section("Add local account") {
                    TextField("Label", text: $newLabel)
                    Button {
                        isAdding = true
                        model.addKeyedAccount(label: newLabel.isEmpty ? "account" : newLabel)
                        isAdding = false
                    } label: {
                        if isAdding {
                            ProgressView()
                        } else {
                            Text("Add + activate")
                        }
                    }
                    .disabled(isAdding)
                }

                Section("Add public-key-only account") {
                    TextField("Label", text: $publicKeyOnlyLabel)
                    Button("Add + select demo account") {
                        model.addPublicKeyOnlyAccount(label: publicKeyOnlyLabel)
                    }
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

    private func shortKey(_ bytes: Data) -> String {
        let hex = bytes.map { String(format: "%02x", $0) }.joined()
        return "\(hex.prefix(8))…\(hex.suffix(8))"
    }
}
