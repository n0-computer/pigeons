import Foundation

/// Everything the user configures. Persisted as JSON in `UserDefaults`.
struct AppSettings: Codable, Equatable {
    /// Endpoint ID of the one roost this app talks to.
    var roostID: String = ""
    /// Unix user to log in as on the roost.
    var user: String = ""
    /// `work.sh`'s `$CODE_DIR` on the roost.
    var codeDir: String = "~/code"
    /// Where `work.sh` lives on the roost.
    var workScript: String = "~/work.sh"
    /// Turn on tmux mouse mode when attaching, so a drag scrolls a pane.
    var tmuxMouse: Bool = true
    /// tmux prefix in tmux notation, e.g. `C-b` or `C-a`.
    var prefix: String = "C-b"
    var fontSize: Double = 13

    var isConfigured: Bool {
        !roostID.trimmingCharacters(in: .whitespaces).isEmpty
            && !user.trimmingCharacters(in: .whitespaces).isEmpty
    }

    /// The parts that require a new connection when they change.
    var connectionKey: String { "\(roostID.trimmingCharacters(in: .whitespaces))@\(user.trimmingCharacters(in: .whitespaces))" }
}

@MainActor
final class SettingsStore: ObservableObject {
    private static let key = "pigeons.settings"

    @Published var settings: AppSettings {
        didSet { persist() }
    }

    init() {
        if let data = UserDefaults.standard.data(forKey: Self.key),
           let saved = try? JSONDecoder().decode(AppSettings.self, from: data) {
            settings = saved
        } else {
            settings = AppSettings()
        }
    }

    private func persist() {
        if let data = try? JSONEncoder().encode(settings) {
            UserDefaults.standard.set(data, forKey: Self.key)
        }
    }
}
