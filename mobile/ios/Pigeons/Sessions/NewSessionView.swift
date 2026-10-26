import SwiftUI

/// `work.sh <project> <tree>` from the phone: pick a project, then an
/// existing worktree, or type a branch name to create one.
struct NewSessionView: View {
    let overview: Overview
    let open: (TerminalRoute) -> Void

    @EnvironmentObject private var client: RoostClient
    @Environment(\.dismiss) private var dismiss
    @State private var project: String = ""
    @State private var worktrees: [Worktree] = []
    @State private var loading = false
    @State private var error: String?
    @State private var branch = ""

    var body: some View {
        NavigationStack {
            Form {
                Section("Project") {
                    Picker("Project", selection: $project) {
                        ForEach(overview.projects, id: \.self) { Text($0).tag($0) }
                    }
                    .pickerStyle(.menu)
                }
                Section {
                    if loading {
                        ProgressView()
                    } else if let error {
                        Text(error).foregroundStyle(.red).font(.footnote)
                    } else if worktrees.isEmpty {
                        Text("No worktrees").foregroundStyle(.secondary)
                    }
                    ForEach(worktrees) { tree in
                        Button {
                            open(.worktree(project: project, tree: tree.name))
                        } label: {
                            HStack {
                                VStack(alignment: .leading, spacing: 2) {
                                    Text(tree.name).font(.body.monospaced())
                                    if let branch = tree.branch, branch != tree.name {
                                        Text(branch).font(.caption).foregroundStyle(.secondary)
                                    }
                                }
                                Spacer()
                                if overview.hasSession(named: tree.session) {
                                    Text("running")
                                        .font(.caption2)
                                        .padding(.horizontal, 6)
                                        .padding(.vertical, 2)
                                        .background(Color.green.opacity(0.2), in: Capsule())
                                }
                            }
                        }
                        .tint(.primary)
                    }
                } header: {
                    Text("Worktrees")
                } footer: {
                    Text("Opening a worktree runs work.sh, which starts its tmux session if it is not running.")
                }
                Section {
                    TextField("branch name, e.g. feat/thing", text: $branch)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .font(.body.monospaced())
                    Button("Create worktree and open") {
                        open(.worktree(project: project, tree: branch.trimmingCharacters(in: .whitespaces)))
                    }
                    .disabled(project.isEmpty || branch.trimmingCharacters(in: .whitespaces).isEmpty)
                } header: {
                    Text("New worktree")
                } footer: {
                    Text("work.sh checks the branch out from origin when it exists, and branches from the default branch when it does not.")
                }
            }
            .navigationTitle("Open")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
            }
            .onAppear {
                if project.isEmpty, let first = overview.projects.first {
                    project = first
                }
            }
            .task(id: project) {
                await loadWorktrees()
            }
        }
    }

    private func loadWorktrees() async {
        guard !project.isEmpty else { return }
        loading = true
        defer { loading = false }
        do {
            worktrees = try await client.listWorktrees(project: project)
            error = nil
        } catch {
            worktrees = []
            self.error = error.localizedDescription
        }
    }
}
