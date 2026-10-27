import SwiftUI

/// tmux sessions on the roost, grouped by project like `work.sh`'s menu.
struct SessionListView: View {
    @EnvironmentObject private var client: RoostClient
    @EnvironmentObject private var store: SettingsStore

    @State private var overview = Overview.empty
    @State private var loading = false
    @State private var loadError: String?
    @State private var showNew = false
    @State private var showNewMenu = false
    @State private var pendingNew: NewChoice?
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
                            NavigationLink(value: TerminalRoute.session(session)) {
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
                ToolbarItem(placement: .topBarTrailing) {
                    Button {
                        showNewMenu = true
                    } label: {
                        Image(systemName: "plus")
                    }
                    .disabled(client.state != .connected)
                    .popover(isPresented: $showNewMenu, arrowEdge: .top) {
                        NewChoicePicker { choice in
                            pendingNew = choice
                            showNewMenu = false
                        }
                        .presentationCompactAdaptation(.popover)
                        // Presenting the sheet while the popover is still
                        // dismissing gets dropped, so act once it has gone.
                        .onDisappear(perform: runPendingNew)
                    }
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

    private func runPendingNew() {
        guard let choice = pendingNew else { return }
        pendingNew = nil
        switch choice {
        case .worktree: showNew = true
        case .shell: path.append(TerminalRoute.shell)
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

/// What the + button offers.
private enum NewChoice: CaseIterable, Identifiable {
    case worktree
    case shell

    var id: Self { self }

    var label: String {
        switch self {
        case .worktree: "Work session"
        case .shell: "Shell"
        }
    }

    var detail: String {
        switch self {
        case .worktree: "Open or create a worktree with work.sh"
        case .shell: "A plain login shell on the roost"
        }
    }

    var icon: String {
        switch self {
        case .worktree: "folder.badge.plus"
        case .shell: "terminal"
        }
    }
}

/// The + popover: same shape as the terminal's pane switcher.
private struct NewChoicePicker: View {
    let choose: (NewChoice) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(NewChoice.allCases) { choice in
                Button {
                    choose(choice)
                } label: {
                    HStack(spacing: 12) {
                        Image(systemName: choice.icon)
                            .frame(width: 24)
                        VStack(alignment: .leading, spacing: 2) {
                            Text(choice.label)
                            Text(choice.detail)
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                        Spacer(minLength: 0)
                    }
                    .contentShape(Rectangle())
                    .padding(.horizontal, 16)
                    .padding(.vertical, 12)
                }
                .buttonStyle(.plain)
                if choice != NewChoice.allCases.last {
                    Divider()
                }
            }
        }
        .frame(minWidth: 240)
        .padding(.vertical, 4)
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
