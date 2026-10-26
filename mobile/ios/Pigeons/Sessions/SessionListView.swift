import SwiftUI

/// tmux sessions on the roost, grouped by project like `work.sh`'s menu.
struct SessionListView: View {
    @EnvironmentObject private var client: RoostClient
    @EnvironmentObject private var store: SettingsStore

    @State private var overview = Overview.empty
    @State private var loading = false
    @State private var loadError: String?
    @State private var showNew = false
    @State private var showSettings = false
    @State private var killTarget: TmuxSession?
    @State private var path = NavigationPath()

    var body: some View {
        NavigationStack(path: $path) {
            List {
                statusSection
                if overview.sessions.isEmpty, client.state == .connected, !loading {
                    ContentUnavailableView(
                        "No tmux sessions",
                        systemImage: "terminal",
                        description: Text("Tap + to open a worktree with work.sh, or pull to refresh.")
                    )
                }
                ForEach(overview.groups) { group in
                    Section(group.project) {
                        ForEach(group.sessions) { session in
                            NavigationLink(value: TerminalRoute.session(session.name)) {
                                SessionRow(session: session)
                            }
                            .swipeActions(edge: .trailing) {
                                Button(role: .destructive) {
                                    killTarget = session
                                } label: {
                                    Label("Kill", systemImage: "xmark.octagon")
                                }
                            }
                        }
                    }
                }
            }
            .listStyle(.insetGrouped)
            .navigationTitle("Sessions")
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    Button {
                        showSettings = true
                    } label: {
                        Image(systemName: "gearshape")
                    }
                }
                ToolbarItemGroup(placement: .topBarTrailing) {
                    Button {
                        path.append(TerminalRoute.shell)
                    } label: {
                        Image(systemName: "terminal")
                    }
                    .disabled(client.state != .connected)
                    Button {
                        showNew = true
                    } label: {
                        Image(systemName: "plus")
                    }
                    .disabled(client.state != .connected)
                }
            }
            .refreshable { await load() }
            .navigationDestination(for: TerminalRoute.self) { route in
                TerminalScreen(route: route, client: client, settings: store.settings)
            }
            .sheet(isPresented: $showNew) {
                NewSessionView(overview: overview) { route in
                    showNew = false
                    path.append(route)
                }
            }
            .sheet(isPresented: $showSettings) {
                NavigationStack { SettingsView() }
            }
            .confirmationDialog(
                "Kill \(killTarget?.name ?? "")? Everything running in it stops.",
                isPresented: Binding(get: { killTarget != nil }, set: { if !$0 { killTarget = nil } }),
                titleVisibility: .visible
            ) {
                Button("Kill session", role: .destructive) {
                    if let session = killTarget {
                        Task { await kill(session) }
                    }
                }
            }
            .task(id: client.state) {
                if client.state == .connected {
                    await load()
                }
            }
            .onChange(of: path) { old, new in
                // Back from a terminal: the list is probably stale.
                if new.count < old.count, client.state == .connected {
                    Task { await load() }
                }
            }
        }
    }

    @ViewBuilder
    private var statusSection: some View {
        Section {
            HStack {
                Circle()
                    .fill(statusColor)
                    .frame(width: 10, height: 10)
                VStack(alignment: .leading, spacing: 2) {
                    Text(client.state.label)
                    if let detail = client.stateDetail, client.state != .connected, !detail.isEmpty {
                        Text(detail)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(3)
                    }
                    if let loadError {
                        Text(loadError)
                            .font(.caption)
                            .foregroundStyle(.red)
                            .lineLimit(3)
                    }
                }
                Spacer()
                if client.state.isBusy || loading {
                    ProgressView()
                } else if client.state != .connected {
                    Button("Connect") {
                        client.reconnectIfNeeded(store.settings)
                    }
                    .buttonStyle(.bordered)
                }
            }
        }
    }

    private var statusColor: Color {
        switch client.state {
        case .connected: .green
        case .dialing, .authenticating: .yellow
        case .failed: .red
        case .disconnected: .gray
        }
    }

    private func load() async {
        guard client.state == .connected else { return }
        loading = true
        defer { loading = false }
        do {
            overview = try await client.listSessions()
            loadError = nil
        } catch {
            loadError = error.localizedDescription
        }
    }

    private func kill(_ session: TmuxSession) async {
        do {
            let result = try await client.killSession(session.name)
            if !result.succeeded {
                loadError = result.message
            }
        } catch {
            loadError = error.localizedDescription
        }
        await load()
    }
}

private struct SessionRow: View {
    let session: TmuxSession

    var body: some View {
        HStack {
            VStack(alignment: .leading, spacing: 3) {
                Text(session.title)
                    .font(.body.monospaced())
                HStack(spacing: 8) {
                    Text("\(session.windows) window\(session.windows == 1 ? "" : "s")")
                    Text(session.activityDate, style: .relative)
                }
                .font(.caption)
                .foregroundStyle(.secondary)
            }
            Spacer()
            if session.isAttached {
                Image(systemName: "eye.fill")
                    .foregroundStyle(.green)
                    .accessibilityLabel("attached elsewhere")
            }
        }
    }
}
