import SwiftUI

/// What the screen shows: the live terminal, or the files around it.
enum PaneMode: String, CaseIterable, Identifiable {
    case terminal, files

    var id: String { rawValue }
    var label: String { self == .terminal ? "Terminal" : "Files" }
    var icon: String { self == .terminal ? "terminal" : "folder" }
}

/// The terminal, the key bar, and the ways out: detach, kill, close. A
/// title-bar toggle swaps the terminal for a file browser scoped to the
/// session's directory; the terminal stays alive underneath.
struct TerminalScreen: View {
    @StateObject private var controller: TerminalController
    @StateObject private var files: FileBrowserModel
    @Environment(\.dismiss) private var dismiss
    @State private var confirmKill = false
    @State private var killError: String?
    @State private var mode = PaneMode.terminal
    @State private var showModePicker = false

    init(route: TerminalRoute, client: RoostClient, settings: AppSettings) {
        _controller = StateObject(wrappedValue: TerminalController(route: route, client: client, settings: settings))
        _files = StateObject(wrappedValue: FileBrowserModel(scope: route.fileScope(codeDir: settings.codeDir), client: client))
    }

    var body: some View {
        ZStack {
            VStack(spacing: 0) {
                TerminalContainer(controller: controller)
                    .background(Color.black)
                TmuxKeyBar(controller: controller)
            }
            .opacity(mode == .terminal ? 1 : 0)
            .allowsHitTesting(mode == .terminal)
            .accessibilityHidden(mode != .terminal)

            if mode == .files {
                FileBrowserView(model: files)
                    .environment(\.colorScheme, .dark)
            }
        }
        .background(Color.black.ignoresSafeArea())
        .navigationTitle(controller.route.title)
        .navigationBarTitleDisplayMode(.inline)
        .toolbarBackground(Color(white: 0.08), for: .navigationBar)
        .toolbarColorScheme(.dark, for: .navigationBar)
        .navigationDestination(for: FileRoute.self) { route in
            FileViewerScreen(route: route)
        }
        .toolbar {
            ToolbarItemGroup(placement: .topBarTrailing) {
                Button {
                    showModePicker = true
                } label: {
                    Image(systemName: mode.icon)
                }
                .accessibilityLabel("Switch between terminal and files")
                .popover(isPresented: $showModePicker, arrowEdge: .top) {
                    PaneModePicker(mode: $mode) { showModePicker = false }
                        .presentationCompactAdaptation(.popover)
                }
                Menu {
                    Button {
                        controller.detach()
                    } label: {
                        Label("Detach", systemImage: "rectangle.portrait.and.arrow.right")
                    }
                    if controller.route.sessionName != nil {
                        Button(role: .destructive) {
                            confirmKill = true
                        } label: {
                            Label("Kill session…", systemImage: "xmark.octagon")
                        }
                    }
                    Button(role: .destructive) {
                        controller.close()
                    } label: {
                        Label("Close terminal", systemImage: "xmark")
                    }
                } label: {
                    Image(systemName: "ellipsis.circle")
                }
            }
        }
        .confirmationDialog(
            "Kill \(controller.route.sessionName ?? "")? Everything running in it stops.",
            isPresented: $confirmKill,
            titleVisibility: .visible
        ) {
            Button("Kill session", role: .destructive) {
                Task {
                    do {
                        if let result = try await controller.killSession(), !result.succeeded {
                            killError = result.message
                        }
                    } catch {
                        killError = error.localizedDescription
                    }
                }
            }
        }
        .alert("Could not kill the session", isPresented: Binding(get: { killError != nil }, set: { if !$0 { killError = nil } })) {
            Button("OK") { killError = nil }
        } message: {
            Text(killError ?? "")
        }
        .overlay {
            if let exit = controller.exit, !exit.isClean {
                ExitOverlay(exit: exit) { dismiss() }
            }
        }
        .onChange(of: controller.exit) { _, exit in
            // A clean exit is a detach or an `exit`: go straight back to the list.
            if let exit, exit.isClean {
                dismiss()
            }
        }
        .onChange(of: mode) { _, mode in
            if mode == .files {
                controller.hideKeyboard()
            }
        }
    }
}

/// The popover under the title-bar toggle.
private struct PaneModePicker: View {
    @Binding var mode: PaneMode
    let done: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(PaneMode.allCases) { candidate in
                Button {
                    mode = candidate
                    done()
                } label: {
                    HStack {
                        Label(candidate.label, systemImage: candidate.icon)
                        Spacer()
                        if candidate == mode {
                            Image(systemName: "checkmark")
                                .foregroundStyle(.tint)
                        }
                    }
                    .contentShape(Rectangle())
                    .padding(.horizontal, 16)
                    .padding(.vertical, 12)
                }
                .buttonStyle(.plain)
                if candidate != PaneMode.allCases.last {
                    Divider()
                }
            }
        }
        .frame(minWidth: 200)
        .padding(.vertical, 4)
        .preferredColorScheme(.dark)
    }
}

private struct ExitOverlay: View {
    let exit: TerminalExit
    let back: () -> Void

    var body: some View {
        VStack(spacing: 12) {
            Image(systemName: "bird")
                .font(.largeTitle)
            Text("Session ended")
                .font(.headline)
            if let error = exit.error {
                Text(error)
                    .font(.footnote)
                    .multilineTextAlignment(.center)
            } else if let code = exit.exitCode {
                Text("tmux exited with status \(code)")
                    .font(.footnote)
            }
            Button("Back to sessions", action: back)
                .buttonStyle(.borderedProminent)
        }
        .padding(24)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 16))
        .padding()
        .preferredColorScheme(.dark)
    }
}
