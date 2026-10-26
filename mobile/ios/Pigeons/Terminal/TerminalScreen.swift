import SwiftUI

/// The terminal, the key bar, and the ways out: detach, kill, close.
struct TerminalScreen: View {
    @StateObject private var controller: TerminalController
    @Environment(\.dismiss) private var dismiss
    @State private var confirmKill = false
    @State private var killError: String?

    init(route: TerminalRoute, client: RoostClient, settings: AppSettings) {
        _controller = StateObject(wrappedValue: TerminalController(route: route, client: client, settings: settings))
    }

    var body: some View {
        VStack(spacing: 0) {
            TerminalContainer(controller: controller)
                .background(Color.black)
            TmuxKeyBar(controller: controller)
        }
        .background(Color.black.ignoresSafeArea())
        .navigationTitle(controller.route.title)
        .navigationBarTitleDisplayMode(.inline)
        .toolbarBackground(Color(white: 0.08), for: .navigationBar)
        .toolbarColorScheme(.dark, for: .navigationBar)
        .toolbar {
            ToolbarItem(placement: .topBarTrailing) {
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
