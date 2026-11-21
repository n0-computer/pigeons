import SwiftUI

/// Machines (each a roost with its own user, paths and tmux prefix), the
/// current connection, per-device display settings, and this device's key.
struct SettingsView: View {
    @EnvironmentObject private var client: RoostClient
    @EnvironmentObject private var store: SettingsStore
    @Environment(\.dismiss) private var dismiss

    @State private var path: [UUID] = []

    var body: some View {
        NavigationStack(path: $path) {
            Form {
                machinesSection
                connectionSection

                Section {
                    Stepper("Font size \(Int(store.settings.fontSize))", value: $store.settings.fontSize, in: 9...20)
                } header: {
                    Text("Display")
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
                    Text("Append the key to ~/.ssh/authorized_keys for your user on each machine. The same ed25519 key is this device's iroh identity.")
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
            .navigationDestination(for: UUID.self) { id in
                RoostEditView(id: id)
            }
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }
                        .disabled(!store.settings.isConfigured)
                }
            }
            .onAppear {
                // First run: straight into the editor for the first machine.
                if store.settings.roosts.isEmpty {
                    addMachine()
                }
            }
        }
    }

    private var machinesSection: some View {
        Section {
            ForEach(store.settings.roosts) { roost in
                HStack {
                    Button {
                        store.settings.select(roost.id)
                    } label: {
                        HStack {
                            Image(systemName: "checkmark")
                                .foregroundStyle(.tint)
                                .opacity(roost.id == store.settings.currentID ? 1 : 0)
                            VStack(alignment: .leading, spacing: 2) {
                                Text(roost.displayName)
                                if !roost.user.isEmpty || !roost.roostID.isEmpty {
                                    Text("\(roost.user)@\(roost.roostID)")
                                        .font(.caption.monospaced())
                                        .foregroundStyle(.secondary)
                                        .lineLimit(1)
                                        .truncationMode(.middle)
                                }
                            }
                        }
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    Spacer()
                    Button {
                        path.append(roost.id)
                    } label: {
                        Image(systemName: "info.circle")
                    }
                    .buttonStyle(.borderless)
                    .accessibilityLabel("Edit \(roost.displayName)")
                }
                .swipeActions(edge: .trailing) {
                    Button(role: .destructive) {
                        store.settings.removeRoost(roost.id)
                    } label: {
                        Label("Remove", systemImage: "trash")
                    }
                }
            }
            Button {
                addMachine()
            } label: {
                Label("Add machine", systemImage: "plus")
            }
        } header: {
            Text("Machines")
        } footer: {
            Text("Tap a machine to switch to it; the session list's title does the same. Each machine is a roost with its own user, work.sh layout and tmux prefix.")
        }
    }

    private var connectionSection: some View {
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
            Text("Connection to \(store.settings.current.displayName)")
        }
    }

    private func addMachine() {
        let roost = store.settings.addRoost()
        path.append(roost.id)
    }
}

/// One machine's settings, plus the password flow that installs this
/// device's key on it.
struct RoostEditView: View {
    let id: UUID

    @EnvironmentObject private var client: RoostClient
    @EnvironmentObject private var store: SettingsStore
    @Environment(\.dismiss) private var dismiss

    @State private var password = ""
    @State private var installing = false
    @State private var installMessage: String?
    @State private var confirmRemove = false

    /// A removed machine reads as blank; writing it back would re-add it,
    /// which only an editor still on its way out could do.
    private var roost: Binding<RoostConfig> {
        Binding(
            get: { store.settings.roost(id) ?? RoostConfig(id: id) },
            set: { store.settings.update($0) }
        )
    }
    private var isCurrent: Bool { store.settings.currentID == id }

    var body: some View {
        Form {
            Section {
                TextField("name, e.g. laptop", text: roost.name)
                    .autocorrectionDisabled()
            } header: {
                Text("Machine")
            } footer: {
                Text("Shown in the list and as the session screen's title.")
            }

            Section {
                TextField("roost endpoint id", text: roost.roostID, axis: .vertical)
                    .font(.footnote.monospaced())
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                TextField("user on the roost", text: roost.user)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
            } header: {
                Text("Roost")
            } footer: {
                Text("The id `pigeons roost` printed on the machine you want to reach.")
            }

            Section {
                if isCurrent {
                    HStack {
                        Text(client.state.label)
                        Spacer()
                        if client.state.isBusy {
                            ProgressView()
                        } else if client.state == .connected {
                            Button("Disconnect") { client.disconnect() }
                        } else {
                            Button("Connect") { client.reconnectIfNeeded(store.settings) }
                                .disabled(!roost.wrappedValue.isConfigured)
                        }
                    }
                    if let detail = client.stateDetail, client.state == .failed {
                        Text(detail).font(.footnote).foregroundStyle(.red)
                    }
                } else {
                    Button("Use this machine") {
                        store.settings.select(id)
                    }
                    .disabled(!roost.wrappedValue.isConfigured)
                }
            } header: {
                Text("Connection")
            }

            Section {
                TextField("code directory", text: roost.codeDir)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .font(.body.monospaced())
                TextField("work.sh path", text: roost.workScript)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .font(.body.monospaced())
            } header: {
                Text("work.sh")
            } footer: {
                Text("Paths on the roost. ~ expands there.")
            }

            Section {
                TextField("prefix (C-b)", text: roost.prefix)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .font(.body.monospaced())
                Toggle("Mouse mode on attach", isOn: roost.tmuxMouse)
            } header: {
                Text("tmux")
            } footer: {
                Text("Mouse mode is what lets a one-finger drag scroll a pane. It runs `set -g mouse on` on the roost's tmux server when attaching.")
            }

            Section {
                SecureField("password for \(roost.wrappedValue.user.isEmpty ? "user" : roost.wrappedValue.user)", text: $password)
                Button {
                    Task { await installKey() }
                } label: {
                    if installing {
                        ProgressView()
                    } else {
                        Text("Log in with password and install key")
                    }
                }
                .disabled(installing || password.isEmpty || !roost.wrappedValue.isConfigured)
                if let installMessage {
                    Text(installMessage).font(.footnote)
                }
            } header: {
                Text("First connection")
            } footer: {
                Text("Alternative to pasting the key by hand: connect once with your password and the app appends its key to authorized_keys on this machine, which becomes the current one. The password is used for that one login and not stored.")
            }

            Section {
                Button("Remove machine", role: .destructive) {
                    confirmRemove = true
                }
            }
        }
        .navigationTitle(roost.wrappedValue.displayName)
        .navigationBarTitleDisplayMode(.inline)
        .confirmationDialog(
            "Remove \(roost.wrappedValue.displayName)? Nothing on the machine changes.",
            isPresented: $confirmRemove,
            titleVisibility: .visible
        ) {
            Button("Remove", role: .destructive) {
                store.settings.removeRoost(id)
                dismiss()
            }
        }
    }

    private func installKey() async {
        installing = true
        defer {
            installing = false
            password = ""
        }
        // Connecting with a password makes this the machine the app talks to.
        store.settings.select(id)
        client.connect(to: store.settings, password: password)
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
