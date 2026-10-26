import SwiftUI

/// Roost, user, work.sh layout, tmux prefix, and this device's key.
struct SettingsView: View {
    @EnvironmentObject private var client: RoostClient
    @EnvironmentObject private var store: SettingsStore
    @Environment(\.dismiss) private var dismiss

    @State private var password = ""
    @State private var installing = false
    @State private var installMessage: String?

    var body: some View {
        Form {
            Section {
                TextField("roost endpoint id", text: $store.settings.roostID, axis: .vertical)
                    .font(.footnote.monospaced())
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                TextField("user on the roost", text: $store.settings.user)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
            } header: {
                Text("Roost")
            } footer: {
                Text("The id `pigeons roost` printed on the machine you want to reach.")
            }

            Section {
                HStack {
                    Text(client.state.label)
                    Spacer()
                    if client.state.isBusy {
                        ProgressView()
                    } else if client.state == .connected {
                        Button("Disconnect") { client.disconnect() }
                    } else {
                        Button("Connect") { client.reconnectIfNeeded(store.settings) }
                            .disabled(!store.settings.isConfigured)
                    }
                }
                if let detail = client.stateDetail, client.state == .failed {
                    Text(detail).font(.footnote).foregroundStyle(.red)
                }
            } header: {
                Text("Connection")
            }

            Section {
                TextField("code directory", text: $store.settings.codeDir)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .font(.body.monospaced())
                TextField("work.sh path", text: $store.settings.workScript)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .font(.body.monospaced())
            } header: {
                Text("work.sh")
            } footer: {
                Text("Paths on the roost. ~ expands there.")
            }

            Section {
                TextField("prefix (C-b)", text: $store.settings.prefix)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .font(.body.monospaced())
                Toggle("Mouse mode on attach", isOn: $store.settings.tmuxMouse)
                Stepper("Font size \(Int(store.settings.fontSize))", value: $store.settings.fontSize, in: 9...20)
            } header: {
                Text("tmux")
            } footer: {
                Text("Mouse mode is what lets a one-finger drag scroll a pane. It runs `set -g mouse on` on the roost's tmux server when attaching.")
            }

            Section {
                LabeledContent("Endpoint") {
                    Text(client.endpointID)
                        .font(.caption2.monospaced())
                        .lineLimit(2)
                        .textSelection(.enabled)
                }
                Text(client.authorizedKey)
                    .font(.caption2.monospaced())
                    .textSelection(.enabled)
                HStack {
                    Button {
                        UIPasteboard.general.string = client.authorizedKey
                    } label: {
                        Label("Copy key", systemImage: "doc.on.doc")
                    }
                    Spacer()
                    ShareLink(item: client.authorizedKey) {
                        Label("Share", systemImage: "square.and.arrow.up")
                    }
                }
            } header: {
                Text("This device")
            } footer: {
                Text("Append the key to ~/.ssh/authorized_keys for your user on the roost. The same ed25519 key is this device's iroh identity.")
            }

            Section {
                SecureField("password for \(store.settings.user.isEmpty ? "user" : store.settings.user)", text: $password)
                Button {
                    Task { await installKey() }
                } label: {
                    if installing {
                        ProgressView()
                    } else {
                        Text("Log in with password and install key")
                    }
                }
                .disabled(installing || password.isEmpty || !store.settings.isConfigured)
                if let installMessage {
                    Text(installMessage).font(.footnote)
                }
            } header: {
                Text("First connection")
            } footer: {
                Text("Alternative to pasting the key by hand: connect once with your password and the app appends its key to authorized_keys. The password is used for that one login and not stored.")
            }

            Section {
                NavigationLink("Core log") { LogView() }
                LabeledContent("Core", value: client.version)
            } header: {
                Text("Debug")
            }
        }
        .navigationTitle("Settings")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .confirmationAction) {
                Button("Done") { dismiss() }
            }
        }
    }

    private func installKey() async {
        installing = true
        defer {
            installing = false
            password = ""
        }
        client.applySettings(store.settings)
        client.connect(roostID: store.settings.roostID, user: store.settings.user, password: password)
        // Wait for this attempt to settle.
        for await state in client.$state.values {
            if state == .connected { break }
            if state == .failed || state == .disconnected {
                installMessage = client.stateDetail ?? "connection failed"
                return
            }
        }
        do {
            let result = try await client.installAuthorizedKey()
            installMessage = result.succeeded
                ? "Key installed. Future connections use it."
                : "The roost refused: \(result.message)"
        } catch {
            installMessage = error.localizedDescription
        }
    }
}
