import SwiftUI

@main
struct PigeonsApp: App {
    @StateObject private var store = SettingsStore()
    @StateObject private var core = CoreLoader()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            RootView()
                .environmentObject(store)
                .environmentObject(core)
                .task { await core.start() }
                .onChange(of: scenePhase) { _, phase in
                    // iOS drops sockets in the background; coming back, dial again.
                    if phase == .active {
                        core.client?.reconnectIfNeeded(store.settings)
                    }
                }
        }
    }
}

/// Owns the core's startup, which binds the iroh endpoint and can fail.
@MainActor
final class CoreLoader: ObservableObject {
    @Published private(set) var client: RoostClient?
    @Published private(set) var error: String?

    func start() async {
        guard client == nil else { return }
        do {
            let support = try FileManager.default.url(for: .applicationSupportDirectory, in: .userDomainMask, appropriateFor: nil, create: true)
            client = try await RoostClient.start(dataDir: support.appendingPathComponent("pigeons", isDirectory: true))
            error = nil
        } catch {
            self.error = error.localizedDescription
        }
    }
}

struct RootView: View {
    @EnvironmentObject private var store: SettingsStore
    @EnvironmentObject private var core: CoreLoader

    var body: some View {
        if let client = core.client {
            ConfiguredRoot(client: client)
                .environmentObject(client)
        } else if let error = core.error {
            ContentUnavailableView {
                Label("Core failed to start", systemImage: "exclamationmark.triangle")
            } description: {
                Text(error)
            } actions: {
                Button("Retry") { Task { await core.start() } }
            }
        } else {
            ProgressView("Starting…")
        }
    }
}

private struct ConfiguredRoot: View {
    @ObservedObject var client: RoostClient
    @EnvironmentObject private var store: SettingsStore
    @State private var onboarding = false
    @State private var connectedKey: String?

    var body: some View {
        SessionListView()
            .sheet(isPresented: $onboarding) {
                NavigationStack { SettingsView() }
                    .interactiveDismissDisabled(!store.settings.isConfigured)
            }
            .onAppear {
                onboarding = !store.settings.isConfigured
                client.reconnectIfNeeded(store.settings)
            }
            .task(id: store.settings.connectionKey) {
                // A different roost or user: start over. The sleep debounces
                // typing in Settings; cancellation restarts the wait.
                let key = store.settings.connectionKey
                guard let previous = connectedKey else {
                    connectedKey = key // first run: onAppear already connected
                    return
                }
                guard key != previous, store.settings.isConfigured else { return }
                try? await Task.sleep(for: .seconds(1))
                guard !Task.isCancelled else { return }
                connectedKey = key
                client.disconnect()
                client.reconnectIfNeeded(store.settings)
            }
            .onChange(of: store.settings) { _, settings in
                client.applySettings(settings)
            }
    }
}
