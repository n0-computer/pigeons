import Foundation

/// Where a terminal's file browser starts and how far up it may go.
struct FileScope: Hashable {
    /// The directory shown first: the tmux session's starting directory.
    let start: String
    /// The browser does not go above this. For a `work.sh` session it is the
    /// project directory that holds every worktree.
    let root: String

    static func forSession(_ session: TmuxSession, codeDir: String) -> FileScope {
        if let project = session.project {
            return FileScope(start: session.path, root: "\(codeDir)/\(project)")
        }
        return FileScope(start: session.path, root: session.path)
    }

    static func forWorktree(project: String, tree: String, codeDir: String) -> FileScope {
        FileScope(start: "\(codeDir)/\(project)/\(tree)", root: "\(codeDir)/\(project)")
    }

    static let home = FileScope(start: "~", root: "/")
}

/// Payload of `pm_client_resolve_path`.
struct ResolvedPath: Codable {
    let path: String
}

struct DirEntry: Codable, Identifiable, Hashable {
    enum Kind: String, Codable {
        case dir, file, other
    }

    let name: String
    let kind: Kind
    let symlink: Bool
    let size: UInt64
    let modified: Int64?

    var id: String { name }
    var isDir: Bool { kind == .dir }
    var isHidden: Bool { name.hasPrefix(".") }
    var modifiedDate: Date? { modified.map { Date(timeIntervalSince1970: TimeInterval($0)) } }
}

struct DirListing: Codable {
    let path: String
    let entries: [DirEntry]
}

struct FileContent: Codable {
    let path: String
    let size: UInt64
    let truncated: Bool
    let binary: Bool
    let text: String
}

/// Pushed onto the navigation stack when a file is opened from the browser.
struct FileRoute: Hashable {
    let path: String
    var name: String { (path as NSString).lastPathComponent }
}

extension UInt64 {
    /// `1.2 KB`, `340 B`, for file lists.
    var byteCountLabel: String {
        ByteCountFormatter.string(fromByteCount: Int64(self), countStyle: .file)
    }
}
