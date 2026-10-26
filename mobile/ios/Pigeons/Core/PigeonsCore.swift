import Foundation
import PigeonsCore

/// Where the connection to the roost stands. Mirrors `PM_STATE_*`.
enum ConnectionState: Int32, Sendable {
    case disconnected = 0
    case dialing = 1
    case authenticating = 2
    case connected = 3
    case failed = 4

    var label: String {
        switch self {
        case .disconnected: "Disconnected"
        case .dialing: "Dialing roost…"
        case .authenticating: "Authenticating…"
        case .connected: "Connected"
        case .failed: "Connection failed"
        }
    }

    var isBusy: Bool { self == .dialing || self == .authenticating }
}

struct CoreError: LocalizedError {
    let message: String
    var errorDescription: String? { message }
}

struct LogLine: Identifiable {
    let id = UUID()
    let date = Date()
    let level: Int32
    let message: String
}

struct TerminalExit: Equatable {
    /// Exit status of the remote command, when sshd reported one.
    let exitCode: Int32?
    /// Set when the terminal ended because of an error rather than the command finishing.
    let error: String?

    var isClean: Bool { error == nil && (exitCode ?? 0) == 0 }
}

/// One open terminal: bytes from the roost come through `onOutput`, bytes
/// typed on the phone go through `write`, and `exit` is set once at the end.
@MainActor
final class TerminalChannel: ObservableObject, Identifiable {
    let id: UInt64
    var onOutput: ((Data) -> Void)?
    @Published private(set) var exit: TerminalExit?
    private weak var client: RoostClient?

    fileprivate init(id: UInt64, client: RoostClient) {
        self.id = id
        self.client = client
    }

    func write(_ bytes: [UInt8]) { client?.terminalWrite(id, bytes) }
    func write(_ text: String) { write(Array(text.utf8)) }
    func resize(cols: Int, rows: Int) { client?.terminalResize(id, cols: cols, rows: rows) }
    func close() { client?.terminalClose(id) }

    fileprivate func receive(_ data: Data) { onOutput?(data) }
    fileprivate func ended(_ exit: TerminalExit) { self.exit = exit }
}

/// Swift face of the Rust core (`mobile/core`): one iroh endpoint for the life
/// of the app and one SSH connection to the configured roost.
///
/// The C ABI calls back on background threads; every callback hops to the
/// main queue here, in order, so published state and terminal output stay
/// consistent for the UI.
@MainActor
final class RoostClient: ObservableObject {
    @Published private(set) var state: ConnectionState = .disconnected
    @Published private(set) var stateDetail: String?
    @Published private(set) var log: [LogLine] = []

    /// This device's iroh endpoint ID.
    let endpointID: String
    /// The `authorized_keys` line that lets this device in.
    let authorizedKey: String

    private let handle: OpaquePointer
    private let bridge: Unmanaged<CoreBridge>
    private var pending: [UInt64: CheckedContinuation<String, Error>] = [:]
    private var terminals: [UInt64: TerminalChannel] = [:]

    private init(handle: OpaquePointer, bridge: Unmanaged<CoreBridge>) {
        self.handle = handle
        self.bridge = bridge
        endpointID = takeString(pm_client_endpoint_id(handle)) ?? ""
        authorizedKey = takeString(pm_client_authorized_key(handle)) ?? ""
        bridge.takeUnretainedValue().client = self
    }

    deinit {
        pm_client_free(handle)
        bridge.release()
    }

    /// Create the core with its state in `dataDir`. Binds the iroh endpoint,
    /// which takes a moment, off the main thread.
    static func start(dataDir: URL) async throws -> RoostClient {
        try FileManager.default.createDirectory(at: dataDir, withIntermediateDirectories: true)
        let bridge = Unmanaged.passRetained(CoreBridge())
        let ctx = RawBox(bridge.toOpaque())
        let path = dataDir.path
        let handle: HandleBox = try await Task.detached {
            var callbacks = pm_callbacks(
                ctx: ctx.pointer,
                on_state: stateTrampoline,
                on_result: resultTrampoline,
                on_terminal_output: outputTrampoline,
                on_terminal_exit: exitTrampoline,
                on_log: logTrampoline
            )
            var error: UnsafeMutablePointer<CChar>?
            guard let handle = pm_client_new(path, &callbacks, &error) else {
                let message = error.map { String(cString: $0) } ?? "unknown error"
                pm_string_free(error)
                throw CoreError(message: message)
            }
            return HandleBox(handle)
        }.value
        return RoostClient(handle: handle.pointer, bridge: bridge)
    }

    var version: String { String(cString: pm_version()) }

    // MARK: Connection

    func applySettings(_ settings: AppSettings) {
        settings.codeDir.withCString { codeDir in
            settings.workScript.withCString { workScript in
                var ps = pm_settings(code_dir: codeDir, work_script: workScript, tmux_mouse: settings.tmuxMouse)
                pm_client_set_settings(handle, &ps)
            }
        }
    }

    /// Dial the roost and log in. The device key is tried first; `password`
    /// only when the key is rejected. Watch `state` for the outcome.
    func connect(roostID: String, user: String, password: String? = nil) {
        pm_client_connect(handle, roostID, user, password)
    }

    func disconnect() {
        pm_client_disconnect(handle)
    }

    /// Connect when configured and not already connected or trying to.
    func reconnectIfNeeded(_ settings: AppSettings) {
        guard settings.isConfigured, !state.isBusy, state != .connected else { return }
        applySettings(settings)
        connect(roostID: settings.roostID, user: settings.user)
    }

    // MARK: Requests

    func listSessions() async throws -> Overview {
        try decode(await request { pm_client_list_sessions($0) })
    }

    func listWorktrees(project: String) async throws -> [Worktree] {
        try decode(await request { pm_client_list_worktrees($0, project) })
    }

    func listBranches(project: String) async throws -> Branches {
        try decode(await request { pm_client_list_branches($0, project) })
    }

    func killSession(_ name: String) async throws -> ExecResult {
        try decode(await request { pm_client_kill_session($0, name) })
    }

    func installAuthorizedKey() async throws -> ExecResult {
        try decode(await request { pm_client_install_authorized_key($0) })
    }

    func exec(_ command: String) async throws -> ExecResult {
        try decode(await request { pm_client_exec($0, command) })
    }

    // MARK: Files

    func resolvePath(_ path: String) async throws -> ResolvedPath {
        try decode(await request { pm_client_resolve_path($0, path) })
    }

    func listDir(_ path: String) async throws -> DirListing {
        try decode(await request { pm_client_list_dir($0, path) })
    }

    func readFile(_ path: String, maxBytes: UInt64) async throws -> FileContent {
        try decode(await request { pm_client_read_file($0, path, maxBytes) })
    }

    // MARK: Terminals

    func attach(session: String, cols: Int, rows: Int) -> TerminalChannel {
        register(pm_client_attach_session(handle, session, clampDim(cols), clampDim(rows)))
    }

    /// `base` is the branch a brand-new branch starts from; nil lets work.sh
    /// ask in the terminal.
    func openWorktree(project: String, tree: String, base: String?, cols: Int, rows: Int) -> TerminalChannel {
        register(pm_client_open_worktree(handle, project, tree, base, clampDim(cols), clampDim(rows)))
    }

    func openShell(cols: Int, rows: Int) -> TerminalChannel {
        register(pm_client_open_shell(handle, clampDim(cols), clampDim(rows)))
    }

    fileprivate func terminalWrite(_ id: UInt64, _ bytes: [UInt8]) {
        bytes.withUnsafeBufferPointer { buf in
            pm_terminal_write(handle, id, buf.baseAddress, buf.count)
        }
    }

    fileprivate func terminalResize(_ id: UInt64, cols: Int, rows: Int) {
        pm_terminal_resize(handle, id, clampDim(cols), clampDim(rows))
    }

    fileprivate func terminalClose(_ id: UInt64) {
        pm_terminal_close(handle, id)
    }

    // MARK: Callbacks (main queue)

    fileprivate func handleState(_ state: ConnectionState, _ detail: String?) {
        self.state = state
        stateDetail = detail
    }

    fileprivate func handleResult(_ id: UInt64, ok: Bool, payload: String) {
        guard let continuation = pending.removeValue(forKey: id) else { return }
        if ok {
            continuation.resume(returning: payload)
        } else {
            continuation.resume(throwing: CoreError(message: payload))
        }
    }

    fileprivate func handleOutput(_ id: UInt64, _ data: Data) {
        terminals[id]?.receive(data)
    }

    fileprivate func handleExit(_ id: UInt64, code: Int32, error: String?) {
        let exit = TerminalExit(exitCode: code < 0 ? nil : code, error: error)
        terminals.removeValue(forKey: id)?.ended(exit)
    }

    fileprivate func handleLog(_ level: Int32, _ message: String) {
        log.append(LogLine(level: level, message: message))
        if log.count > 500 {
            log.removeFirst(log.count - 500)
        }
    }

    // MARK: Plumbing

    private func request(_ start: (OpaquePointer) -> UInt64) async throws -> String {
        try await withCheckedThrowingContinuation { continuation in
            let id = start(handle)
            guard id != 0 else {
                continuation.resume(throwing: CoreError(message: "the core rejected the request"))
                return
            }
            // The result callback arrives on the main queue, so it cannot run
            // before this synchronous registration completes.
            pending[id] = continuation
        }
    }

    private func register(_ id: UInt64) -> TerminalChannel {
        let channel = TerminalChannel(id: id, client: self)
        if id == 0 {
            channel.ended(TerminalExit(exitCode: nil, error: "the core rejected the terminal request"))
        } else {
            terminals[id] = channel
        }
        return channel
    }

    private func decode<T: Decodable>(_ json: String) throws -> T {
        do {
            return try JSONDecoder().decode(T.self, from: Data(json.utf8))
        } catch {
            throw CoreError(message: "unexpected payload from the core: \(error.localizedDescription)")
        }
    }
}

/// Pointers crossing the startup task. The core hands them out thread-safe.
private struct HandleBox: @unchecked Sendable {
    let pointer: OpaquePointer
    init(_ pointer: OpaquePointer) { self.pointer = pointer }
}

private struct RawBox: @unchecked Sendable {
    let pointer: UnsafeMutableRawPointer
    init(_ pointer: UnsafeMutableRawPointer) { self.pointer = pointer }
}

private func clampDim(_ v: Int) -> UInt16 {
    UInt16(clamping: max(v, 1))
}

/// Take ownership of a string the core returned.
private func takeString(_ p: UnsafeMutablePointer<CChar>?) -> String? {
    guard let p else { return nil }
    defer { pm_string_free(p) }
    return String(cString: p)
}

/// The `ctx` behind the C callbacks. Not actor-isolated: callbacks read the
/// weak client reference on whatever thread the core is on, then hop.
private final class CoreBridge {
    weak var client: RoostClient?
}

private func onMain(_ ctx: UnsafeMutableRawPointer?, _ body: @escaping @MainActor (RoostClient) -> Void) {
    guard let ctx else { return }
    let bridge = Unmanaged<CoreBridge>.fromOpaque(ctx).takeUnretainedValue()
    // The main queue is FIFO, which keeps terminal output in order; Tasks are not.
    DispatchQueue.main.async {
        MainActor.assumeIsolated {
            if let client = bridge.client {
                body(client)
            }
        }
    }
}

private func stateTrampoline(_ ctx: UnsafeMutableRawPointer?, _ state: Int32, _ detail: UnsafePointer<CChar>?) {
    let detail = detail.map { String(cString: $0) }
    let state = ConnectionState(rawValue: state) ?? .failed
    onMain(ctx) { $0.handleState(state, detail) }
}

private func resultTrampoline(_ ctx: UnsafeMutableRawPointer?, _ id: UInt64, _ ok: Bool, _ payload: UnsafePointer<CChar>?) {
    let payload = payload.map { String(cString: $0) } ?? ""
    onMain(ctx) { $0.handleResult(id, ok: ok, payload: payload) }
}

private func outputTrampoline(_ ctx: UnsafeMutableRawPointer?, _ id: UInt64, _ bytes: UnsafePointer<UInt8>?, _ len: Int) {
    guard let bytes, len > 0 else { return }
    let data = Data(bytes: bytes, count: len)
    onMain(ctx) { $0.handleOutput(id, data) }
}

private func exitTrampoline(_ ctx: UnsafeMutableRawPointer?, _ id: UInt64, _ code: Int32, _ error: UnsafePointer<CChar>?) {
    let error = error.map { String(cString: $0) }
    onMain(ctx) { $0.handleExit(id, code: code, error: error) }
}

private func logTrampoline(_ ctx: UnsafeMutableRawPointer?, _ level: Int32, _ message: UnsafePointer<CChar>?) {
    let message = message.map { String(cString: $0) } ?? ""
    onMain(ctx) { $0.handleLog(level, message) }
}
