import SwiftUI

/// `work.sh <project> <tree>` from the phone: pick a project, then an
/// existing worktree, or type a branch name to create one. A brand-new
/// branch starts from the base picked here (origin's default branch unless
/// changed), handed to the script as `BASE` so it does not have to ask.
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
    @State private var branches: Branches = .empty
    @State private var branchesError: String?
    @State private var base = ""

    private var newBranch: String { branch.trimmingCharacters(in: .whitespaces) }

    /// work.sh checks an existing branch out as it is; the base only matters
    /// for a branch that is nowhere yet.
    private var branchExists: Bool { branches.branches.contains(newBranch) }

    /// What to hand work.sh as `BASE`: nil lets it ask in the terminal.
    private var chosenBase: String? {
        if branchExists || base.isEmpty { return nil }
        return base
    }

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
                            open(.worktree(project: project, tree: tree.name, base: nil))
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
                    if branchExists {
                        LabeledContent("Start from") {
                            Text("existing branch").foregroundStyle(.secondary)
                        }
                    } else if branches.branches.isEmpty {
                        LabeledContent("Start from") {
                            Text(branchesError == nil ? "asked in the terminal" : "asked in the terminal (branch list failed)")
                                .foregroundStyle(.secondary)
                        }
                    } else {
                        Picker("Start from", selection: $base) {
                            ForEach(branches.branches, id: \.self) { name in
                                if name == branches.defaultBranch {
                                    Text("\(name) (default)").tag(name)
                                } else {
                                    Text(name).tag(name)
                                }
                            }
                        }
                        .pickerStyle(.menu)
                        .font(.body.monospaced())
                    }
                    Button("Create worktree and open") {
                        open(.worktree(project: project, tree: newBranch, base: chosenBase))
                    }
                    .disabled(project.isEmpty || newBranch.isEmpty)
                } header: {
                    Text("New worktree")
                } footer: {
                    Text(branchExists
                         ? "\(newBranch) already exists; work.sh checks it out as it is."
                         : "work.sh checks the branch out from origin when it exists there, and otherwise creates it from the branch picked above, preferring origin's copy.")
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
                async let trees: Void = loadWorktrees()
                async let bases: Void = loadBranches()
                _ = await (trees, bases)
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

    private func loadBranches() async {
        guard !project.isEmpty else { return }
        branches = .empty
        base = ""
        do {
            branches = try await client.listBranches(project: project)
            branchesError = nil
            // Default to what work.sh would pick on its own; else the newest.
            base = branches.defaultBranch ?? branches.branches.first ?? ""
        } catch {
            branchesError = error.localizedDescription
        }
    }
}
