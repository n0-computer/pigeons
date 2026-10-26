import SwiftUI

/// Walks the roost's filesystem within a [`FileScope`]: lists one directory
/// at a time, remembers where it is while the user flips back to the
/// terminal, and refuses to climb above the scope's root.
@MainActor
final class FileBrowserModel: ObservableObject {
    let scope: FileScope

    @Published private(set) var root: String?
    @Published private(set) var listing: DirListing?
    @Published private(set) var loading = false
    @Published private(set) var error: String?
    @Published var showHidden = false

    private let client: RoostClient
    private var started = false

    init(scope: FileScope, client: RoostClient) {
        self.scope = scope
        self.client = client
    }

    var path: String? { listing?.path }

    /// `project/tree/src`: the current directory relative to the root's parent,
    /// so the project name stays visible.
    var displayPath: String {
        guard let path, let root else { return "" }
        let rootParent = (root as NSString).deletingLastPathComponent
        if rootParent != "/" && path.hasPrefix(rootParent + "/") {
            return String(path.dropFirst(rootParent.count + 1))
        }
        return path
    }

    var canGoUp: Bool {
        guard let path, let root else { return false }
        return path != root && (path.hasPrefix(root + "/") || root == "/")
    }

    var entries: [DirEntry] {
        guard let listing else { return [] }
        return showHidden ? listing.entries : listing.entries.filter { !$0.isHidden }
    }

    /// First load: resolve the root once, then show the start directory.
    func startIfNeeded() async {
        guard !started else { return }
        started = true
        do {
            root = try await client.resolvePath(scope.root).path
        } catch {
            self.error = error.localizedDescription
            started = false
            return
        }
        await load(scope.start, widenRoot: true)
    }

    func refresh() async {
        if let path {
            await load(path)
        } else {
            started = false
            await startIfNeeded()
        }
    }

    func open(_ entry: DirEntry) async {
        guard entry.isDir, let path else { return }
        await load(path + "/" + entry.name)
    }

    func up() async {
        guard canGoUp, let path else { return }
        await load((path as NSString).deletingLastPathComponent)
    }

    func goToRoot() async {
        if let root { await load(root) }
    }

    func goToStart() async {
        await load(scope.start)
    }

    /// `widenRoot` lets the first directory redefine the root when it is not
    /// inside it, as with a session started somewhere other than its worktree.
    private func load(_ path: String, widenRoot: Bool = false) async {
        loading = true
        defer { loading = false }
        do {
            let next = try await client.listDir(path)
            if let root, next.path != root, !next.path.hasPrefix(root + "/"), root != "/" {
                if widenRoot {
                    self.root = next.path
                } else {
                    // A symlink led outside the scope; stay where we were.
                    error = "\(next.path) is outside \(root)"
                    return
                }
            }
            listing = next
            error = nil
        } catch {
            self.error = error.localizedDescription
        }
    }
}

/// One directory of the roost, with the way up and a way back to the start.
struct FileBrowserView: View {
    @ObservedObject var model: FileBrowserModel

    var body: some View {
        VStack(spacing: 0) {
            pathBar
            if let error = model.error, model.listing == nil {
                ContentUnavailableView("Could not list files", systemImage: "folder.badge.questionmark", description: Text(error))
            } else {
                list
            }
        }
        .background(Color.black)
        .task { await model.startIfNeeded() }
    }

    private var pathBar: some View {
        HStack(spacing: 10) {
            Button {
                Task { await model.up() }
            } label: {
                Image(systemName: "arrow.up")
                    .frame(width: 32, height: 32)
            }
            .disabled(!model.canGoUp)
            .accessibilityLabel("Parent directory")

            ScrollView(.horizontal, showsIndicators: false) {
                Text(model.displayPath)
                    .font(.footnote.monospaced())
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            .defaultScrollAnchor(.trailing)

            if model.loading {
                ProgressView()
                    .controlSize(.small)
            }

            Menu {
                Button {
                    Task { await model.goToStart() }
                } label: {
                    Label("Session directory", systemImage: "terminal")
                }
                Button {
                    Task { await model.goToRoot() }
                } label: {
                    Label("Project root", systemImage: "folder")
                }
                .disabled(!model.canGoUp)
                Toggle(isOn: $model.showHidden) {
                    Label("Hidden files", systemImage: "eye")
                }
                Button {
                    Task { await model.refresh() }
                } label: {
                    Label("Refresh", systemImage: "arrow.clockwise")
                }
            } label: {
                Image(systemName: "ellipsis.circle")
                    .frame(width: 32, height: 32)
            }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 4)
        .background(Color(white: 0.08))
    }

    private var list: some View {
        List {
            if let error = model.error {
                Text(error)
                    .font(.caption)
                    .foregroundStyle(.red)
                    .listRowBackground(Color(white: 0.1))
            }
            if model.entries.isEmpty, !model.loading, model.listing != nil {
                Text("Empty directory")
                    .foregroundStyle(.secondary)
                    .listRowBackground(Color.black)
            }
            ForEach(model.entries) { entry in
                if entry.isDir {
                    Button {
                        Task { await model.open(entry) }
                    } label: {
                        EntryRow(entry: entry)
                    }
                    .listRowBackground(Color.black)
                } else if entry.kind == .file {
                    NavigationLink(value: FileRoute(path: (model.path ?? "") + "/" + entry.name)) {
                        EntryRow(entry: entry)
                    }
                    .listRowBackground(Color.black)
                } else {
                    EntryRow(entry: entry)
                        .foregroundStyle(.secondary)
                        .listRowBackground(Color.black)
                }
            }
        }
        .listStyle(.plain)
        .scrollContentBackground(.hidden)
        .refreshable { await model.refresh() }
    }
}

private struct EntryRow: View {
    let entry: DirEntry

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: icon)
                .foregroundStyle(entry.isDir ? Color.blue : Color.secondary)
                .frame(width: 22)
            Text(entry.name)
                .font(.body.monospaced())
                .lineLimit(1)
                .truncationMode(.middle)
            if entry.symlink {
                Image(systemName: "arrow.turn.down.right")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            if entry.kind == .file {
                Text(entry.size.byteCountLabel)
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
            } else if entry.isDir {
                Image(systemName: "chevron.right")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.tertiary)
            }
        }
        .contentShape(Rectangle())
    }

    private var icon: String {
        switch entry.kind {
        case .dir: "folder.fill"
        case .file: fileIcon
        case .other: "questionmark.square.dashed"
        }
    }

    private var fileIcon: String {
        switch (entry.name as NSString).pathExtension.lowercased() {
        case "md", "markdown", "txt", "rst": "doc.text"
        case "png", "jpg", "jpeg", "gif", "svg", "webp", "heic": "photo"
        case "json", "yaml", "yml", "toml", "plist", "xml": "doc.badge.gearshape"
        case "rs", "swift", "js", "ts", "tsx", "jsx", "py", "go", "c", "h", "cpp", "sh", "rb", "java", "kt": "chevron.left.forwardslash.chevron.right"
        case "lock", "zip", "tar", "gz", "a", "o", "dylib", "so": "shippingbox"
        default: "doc"
        }
    }
}
