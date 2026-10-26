import Combine
import Foundation
import SwiftTerm
import SwiftUI
import UIKit

/// What a terminal screen opens.
enum TerminalRoute: Hashable {
    /// Attach to an existing tmux session.
    case session(String)
    /// `work.sh <project> <tree>`: create the worktree and session if needed, then attach.
    case worktree(project: String, tree: String)
    /// A plain login shell on the roost.
    case shell

    var title: String {
        switch self {
        case .session(let name): name
        case .worktree(let project, let tree): "\(project)/\(tree)"
        case .shell: "shell"
        }
    }

    /// The tmux session a screen is (or will be) attached to, when known.
    var sessionName: String? {
        switch self {
        case .session(let name): name
        case .worktree(let project, let tree): "\(project)/\(tree)".replacingOccurrences(of: ".", with: "-").replacingOccurrences(of: ":", with: "-")
        case .shell: nil
        }
    }
}

/// Glue between one `TmuxTerminalView` and one `TerminalChannel`.
///
/// The PTY is opened lazily, on the first size report from the view, so the
/// remote side starts with the real column and row count instead of a guess
/// that would be corrected a moment later.
@MainActor
final class TerminalController: ObservableObject {
    let route: TerminalRoute
    let prefix: [UInt8]
    let fontSize: CGFloat

    @Published private(set) var exit: TerminalExit?
    @Published private(set) var controlArmed = false
    @Published private(set) var keyboardShown = false
    @Published private(set) var started = false

    private let client: RoostClient
    private var channel: TerminalChannel?
    private weak var view: TmuxTerminalView?
    private var cancellables = Set<AnyCancellable>()

    init(route: TerminalRoute, client: RoostClient, settings: AppSettings) {
        self.route = route
        self.client = client
        prefix = TmuxKeys.prefixBytes(settings.prefix)
        fontSize = CGFloat(settings.fontSize)
    }

    func attach(view: TmuxTerminalView) {
        self.view = view
        view.onControlArmedChanged = { [weak self] armed in self?.controlArmed = armed }
        view.onKeyboardChanged = { [weak self] shown in self?.keyboardShown = shown }
    }

    /// Hide the keyboard to see more of the terminal, or bring it back.
    /// Tapping the terminal also brings it back.
    func toggleKeyboard() {
        guard let view else { return }
        if view.isFirstResponder {
            _ = view.resignFirstResponder()
        } else {
            _ = view.becomeFirstResponder()
        }
    }

    /// The view has a size. Open the PTY with it, or resize the one we have.
    func viewSized(cols: Int, rows: Int) {
        guard cols > 0, rows > 0 else { return }
        if let channel {
            channel.resize(cols: cols, rows: rows)
        } else if !started {
            start(cols: cols, rows: rows)
        }
    }

    /// Keystrokes from the view.
    func send(_ bytes: [UInt8]) {
        channel?.write(bytes)
    }

    func press(_ key: TmuxKeys.Key) {
        view?.press(key, prefix: prefix)
    }

    func detach() {
        press(.prefixed("d"))
    }

    func close() {
        channel?.close()
    }

    func killSession() async throws -> ExecResult? {
        guard let name = route.sessionName else { return nil }
        return try await client.killSession(name)
    }

    private func start(cols: Int, rows: Int) {
        started = true
        let channel: TerminalChannel
        switch route {
        case .session(let name):
            channel = client.attach(session: name, cols: cols, rows: rows)
        case .worktree(let project, let tree):
            channel = client.openWorktree(project: project, tree: tree, cols: cols, rows: rows)
        case .shell:
            channel = client.openShell(cols: cols, rows: rows)
        }
        channel.onOutput = { [weak self] data in
            self?.view?.feed(byteArray: [UInt8](data)[...])
        }
        channel.$exit
            .compactMap { $0 }
            .receive(on: DispatchQueue.main)
            .sink { [weak self] exit in self?.exit = exit }
            .store(in: &cancellables)
        self.channel = channel
        _ = view?.becomeFirstResponder()
    }
}

/// Hosts the UIKit terminal view in SwiftUI and forwards SwiftTerm's delegate
/// callbacks to the controller.
struct TerminalContainer: UIViewRepresentable {
    @ObservedObject var controller: TerminalController

    func makeUIView(context: Context) -> TmuxTerminalView {
        let view = TmuxTerminalView(frame: .zero)
        view.font = UIFont.monospacedSystemFont(ofSize: controller.fontSize, weight: .regular)
        view.terminalDelegate = context.coordinator
        controller.attach(view: view)
        // SwiftTerm only reports a size when it changes from its default, so
        // read the first one ourselves once layout has happened.
        DispatchQueue.main.async {
            let dims = view.terminalDimensions
            controller.viewSized(cols: dims.cols, rows: dims.rows)
        }
        return view
    }

    func updateUIView(_ uiView: TmuxTerminalView, context: Context) {}

    func makeCoordinator() -> Coordinator {
        Coordinator(controller: controller)
    }

    final class Coordinator: NSObject, TerminalViewDelegate {
        let controller: TerminalController

        init(controller: TerminalController) {
            self.controller = controller
        }

        func sizeChanged(source: TerminalView, newCols: Int, newRows: Int) {
            controller.viewSized(cols: newCols, rows: newRows)
        }

        func send(source: TerminalView, data: ArraySlice<UInt8>) {
            controller.send(Array(data))
        }

        func setTerminalTitle(source: TerminalView, title: String) {}
        func hostCurrentDirectoryUpdate(source: TerminalView, directory: String?) {}
        func scrolled(source: TerminalView, position: Double) {}

        func requestOpenLink(source: TerminalView, link: String, params: [String: String]) {
            if let url = URL(string: link) {
                UIApplication.shared.open(url)
            }
        }

        func bell(source: TerminalView) {
            UIImpactFeedbackGenerator(style: .light).impactOccurred()
        }

        /// OSC 52: tmux's `set-clipboard` lands yanked text on the phone's clipboard.
        func clipboardCopy(source: TerminalView, content: Data) {
            if let text = String(data: content, encoding: .utf8) {
                UIPasteboard.general.string = text
            }
        }

        func clipboardRead(source: TerminalView) -> Data? { nil }
        func kittyClipboardCapabilities(source: TerminalView) -> KittyClipboardCapabilities { [] }
        func kittyClipboardAvailableMimeTypes(source: TerminalView, location: KittyClipboardLocation) -> [String]? { nil }
        func kittyClipboardRead(source: TerminalView, location: KittyClipboardLocation, mimeType: String) -> KittyClipboardReadResult? { nil }
        func kittyClipboardWrite(source: TerminalView, location: KittyClipboardLocation, content: KittyClipboardWriteContent) -> KittyClipboardWriteResult { .unsupported }
        func kittyClipboardRequestPermission(source: TerminalView, request: KittyClipboardPermissionRequest) -> KittyClipboardPermissionResult { .deny }
        func iTermContent(source: TerminalView, content: ArraySlice<UInt8>) {}
        func rangeChanged(source: TerminalView, startY: Int, endY: Int) {}
    }
}
