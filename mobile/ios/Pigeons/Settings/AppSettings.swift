import Foundation

/// One machine the app can reach: a roost plus how `work.sh` is laid out
/// there and how its tmux is driven.
struct RoostConfig: Codable, Equatable, Identifiable {
    var id = UUID()
    /// What the list and title show. Falls back to user@id when blank.
    var name: String = ""
    /// Endpoint ID of the roost.
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

    var isConfigured: Bool {
        !roostID.trimmingCharacters(in: .whitespaces).isEmpty
            && !user.trimmingCharacters(in: .whitespaces).isEmpty
    }

    /// The parts that require a new connection when they change.
    var connectionKey: String {
        "\(roostID.trimmingCharacters(in: .whitespaces))@\(user.trimmingCharacters(in: .whitespaces))"
    }

    /// Name for display: the given one, else user@short-id, else a placeholder.
    var displayName: String {
        let trimmed = name.trimmingCharacters(in: .whitespaces)
        if !trimmed.isEmpty { return trimmed }
        let id = roostID.trimmingCharacters(in: .whitespaces)
        if id.isEmpty { return "New machine" }
        let short = id.count > 10 ? String(id.prefix(10)) + "…" : id
        return user.isEmpty ? short : "\(user)@\(short)"
    }
}

/// Everything the user configures: the machines, which one is current, and
/// the per-device bits. Persisted as JSON in `UserDefaults`.
///
/// The current machine's fields are forwarded (`roostID`, `codeDir`, ...) so
/// the rest of the app reads settings as if there were one machine.
struct AppSettings: Codable, Equatable {
    var roosts: [RoostConfig] = []
    var currentID: UUID?
    var fontSize: Double = 13

    // MARK: The current machine

    private static let none = RoostConfig()

    var currentIndex: Int? {
        guard let currentID else { return nil }
        return roosts.firstIndex { $0.id == currentID }
    }

    /// The machine the app talks to. A blank placeholder when there is none,
    /// so callers do not have to special-case the first run.
    var current: RoostConfig {
        currentIndex.map { roosts[$0] } ?? Self.none
    }

    var roostID: String { current.roostID }
    var user: String { current.user }
    var codeDir: String { current.codeDir }
    var workScript: String { current.workScript }
    var tmuxMouse: Bool { current.tmuxMouse }
    var prefix: String { current.prefix }
    var isConfigured: Bool { current.isConfigured }

    /// The parts that require a new connection when they change: which
    /// machine, and its roost and user.
    var connectionKey: String {
        "\(currentID?.uuidString ?? "-"):\(current.connectionKey)"
    }

    // MARK: Editing

    func roost(_ id: UUID) -> RoostConfig? {
        roosts.first { $0.id == id }
    }

    mutating func update(_ roost: RoostConfig) {
        if let i = roosts.firstIndex(where: { $0.id == roost.id }) {
            roosts[i] = roost
        } else {
            roosts.append(roost)
        }
        if currentID == nil { currentID = roost.id }
    }

    /// Append a blank machine and return it; it becomes current when there
    /// was none.
    @discardableResult
    mutating func addRoost() -> RoostConfig {
        let roost = RoostConfig()
        update(roost)
        return roost
    }

    mutating func removeRoost(_ id: UUID) {
        roosts.removeAll { $0.id == id }
        if currentID == id { currentID = roosts.first?.id }
    }

    mutating func select(_ id: UUID) {
        guard roosts.contains(where: { $0.id == id }) else { return }
        currentID = id
    }

    // MARK: Persistence

    private enum CodingKeys: String, CodingKey {
        case roosts, currentID, fontSize
    }

    /// The shape the app stored before it knew about several machines.
    private struct Legacy: Decodable {
        var roostID = ""
        var user = ""
        var codeDir = "~/code"
        var workScript = "~/work.sh"
        var tmuxMouse = true
        var prefix = "C-b"
        var fontSize = 13.0

        private enum Keys: String, CodingKey {
            case roostID, user, codeDir, workScript, tmuxMouse, prefix, fontSize
        }

        init(from decoder: Decoder) throws {
            let c = try decoder.container(keyedBy: Keys.self)
            roostID = try c.decodeIfPresent(String.self, forKey: .roostID) ?? roostID
            user = try c.decodeIfPresent(String.self, forKey: .user) ?? user
            codeDir = try c.decodeIfPresent(String.self, forKey: .codeDir) ?? codeDir
            workScript = try c.decodeIfPresent(String.self, forKey: .workScript) ?? workScript
            tmuxMouse = try c.decodeIfPresent(Bool.self, forKey: .tmuxMouse) ?? tmuxMouse
            prefix = try c.decodeIfPresent(String.self, forKey: .prefix) ?? prefix
            fontSize = try c.decodeIfPresent(Double.self, forKey: .fontSize) ?? fontSize
        }
    }

    init() {}

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        if c.contains(.roosts) {
            roosts = try c.decode([RoostConfig].self, forKey: .roosts)
            currentID = try c.decodeIfPresent(UUID.self, forKey: .currentID)
            fontSize = try c.decodeIfPresent(Double.self, forKey: .fontSize) ?? 13
            if currentID.flatMap(roost) == nil { currentID = roosts.first?.id }
            return
        }
        // One-machine settings from an earlier build: keep them as the first machine.
        let old = try Legacy(from: decoder)
        fontSize = old.fontSize
        var roost = RoostConfig()
        roost.roostID = old.roostID
        roost.user = old.user
        roost.codeDir = old.codeDir
        roost.workScript = old.workScript
        roost.tmuxMouse = old.tmuxMouse
        roost.prefix = old.prefix
        if roost.isConfigured {
            roosts = [roost]
            currentID = roost.id
        }
    }
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
