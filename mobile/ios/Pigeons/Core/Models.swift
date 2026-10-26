import Foundation

/// One tmux session as the core reports it, with the `project/tree` split
/// `work.sh` encodes in session names when the name has that shape.
struct TmuxSession: Codable, Identifiable, Hashable {
    let name: String
    let project: String?
    let tree: String?
    let windows: Int
    let attached: Int
    let created: Int64
    let activity: Int64
    let path: String

    var id: String { name }
    var isAttached: Bool { attached > 0 }
    var title: String { tree ?? name }
    var activityDate: Date { Date(timeIntervalSince1970: TimeInterval(activity)) }
}

/// Sessions grouped by project, the way `work.sh`'s menu lists them.
struct SessionGroup: Identifiable {
    let project: String
    let sessions: [TmuxSession]
    var id: String { project }
}

/// What the session screen shows.
struct Overview: Codable {
    var sessions: [TmuxSession]
    var projects: [String]

    static let empty = Overview(sessions: [], projects: [])

    /// Projects in the order the roost listed them, each with its sessions;
    /// sessions whose name is not `project/tree` land in a final "other" group.
    var groups: [SessionGroup] {
        var byProject: [String: [TmuxSession]] = [:]
        var other: [TmuxSession] = []
        for session in sessions {
            if let project = session.project {
                byProject[project, default: []].append(session)
            } else {
                other.append(session)
            }
        }
        var ordered = projects
        for project in byProject.keys.sorted() where !ordered.contains(project) {
            ordered.append(project)
        }
        var groups = ordered.compactMap { project -> SessionGroup? in
            guard let sessions = byProject[project], !sessions.isEmpty else { return nil }
            return SessionGroup(project: project, sessions: sessions.sorted { $0.activity > $1.activity })
        }
        if !other.isEmpty {
            groups.append(SessionGroup(project: "other", sessions: other.sorted { $0.activity > $1.activity }))
        }
        return groups
    }

    func hasSession(named name: String) -> Bool {
        sessions.contains { $0.name == name }
    }
}

/// A git worktree of a project and the session name `work.sh` gives it.
struct Worktree: Codable, Identifiable, Hashable {
    let name: String
    let path: String
    let branch: String?
    let session: String
    var id: String { path }
}

/// Branches a new worktree can start from, as `pm_client_list_branches`
/// reports them: newest commit first, local and origin merged.
struct Branches: Codable {
    /// What `work.sh` picks when told nothing: origin's HEAD branch, else
    /// `main` or `master`. Nil when the project has neither.
    let defaultBranch: String?
    let branches: [String]

    enum CodingKeys: String, CodingKey {
        case defaultBranch = "default"
        case branches
    }

    static let empty = Branches(defaultBranch: nil, branches: [])
}

/// Result of a command run on the roost.
struct ExecResult: Codable {
    let exitCode: Int?
    let stdout: String
    let stderr: String

    enum CodingKeys: String, CodingKey {
        case exitCode = "exit_code"
        case stdout, stderr
    }

    var succeeded: Bool { exitCode == 0 }
    /// Something to show a person: stderr, else stdout, trimmed.
    var message: String {
        let err = stderr.trimmingCharacters(in: .whitespacesAndNewlines)
        return err.isEmpty ? stdout.trimmingCharacters(in: .whitespacesAndNewlines) : err
    }
}
