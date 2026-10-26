import Foundation

/// The tmux key vocabulary: prefix parsing, control characters, and the
/// buttons the key bar offers above the keyboard.
enum TmuxKeys {
    enum Arrow: Hashable { case up, down, left, right }

    enum Key: Hashable {
        /// Bytes sent as they are.
        case bytes([UInt8])
        /// Literal text, UTF-8 encoded.
        case text(String)
        /// The tmux prefix followed by `key`; an empty key sends the bare prefix.
        case prefixed(String)
        /// Arm Ctrl for the next character typed on the keyboard.
        case controlToggle
        /// Cursor keys, encoded for the terminal's current cursor mode.
        case arrow(Arrow)
    }

    struct Button: Identifiable, Hashable {
        let label: String
        let key: Key
        let hint: String
        var id: String { "\(label)|\(hint)" }

        init(_ label: String, _ key: Key, _ hint: String) {
            self.label = label
            self.key = key
            self.hint = hint
        }
    }

    /// The byte a terminal sends for Ctrl plus `ch`, for the characters that
    /// have one.
    static func control(_ ch: Character) -> UInt8? {
        guard let ascii = ch.asciiValue else { return nil }
        switch ascii {
        case 0x61...0x7a: return ascii - 0x60 // a-z
        case 0x41...0x5a: return ascii - 0x40 // A-Z
        case 0x40, 0x20: return 0x00 // @ and space
        case 0x5b...0x5f: return ascii - 0x40 // [ \ ] ^ _
        case 0x3f: return 0x7f // ?
        default: return nil
        }
    }

    /// Parse a prefix written in tmux notation (`C-b`, `C-a`, `C-Space`, or a
    /// bare key like `` ` ``). Anything unparseable falls back to `C-b`.
    static func prefixBytes(_ notation: String) -> [UInt8] {
        let n = notation.trimmingCharacters(in: .whitespaces)
        if n.lowercased().hasPrefix("c-") {
            let rest = String(n.dropFirst(2))
            if rest.lowercased() == "space" { return [0x00] }
            if rest.count == 1, let first = rest.first, let byte = control(first) { return [byte] }
        } else if n.count == 1, let byte = n.utf8.first {
            return [byte]
        }
        return [0x02]
    }

    /// tmux actions: session and window management, killing things, scrolling.
    static let tmuxRow: [Button] = [
        Button("Detach", .prefixed("d"), "prefix d: leave the session running and go back to the list"),
        Button("Scroll", .prefixed("["), "prefix [: copy mode; drag or arrow to scroll, q to leave"),
        Button("Windows", .prefixed("w"), "prefix w: choose a window"),
        Button("New", .prefixed("c"), "prefix c: new window"),
        Button("Next", .prefixed("n"), "prefix n: next window"),
        Button("Prev", .prefixed("p"), "prefix p: previous window"),
        Button("Zoom", .prefixed("z"), "prefix z: zoom the pane"),
        Button("Kill pane", .prefixed("x"), "prefix x: kill the pane; tmux asks y/n"),
        Button("Kill win", .prefixed("&"), "prefix &: kill the window; tmux asks y/n"),
        Button("Prefix", .prefixed(""), "send the bare prefix, then type any tmux key"),
    ]

    /// Process control and keys the soft keyboard lacks.
    static let controlRow: [Button] = [
        Button("Esc", .bytes([0x1b]), "escape"),
        Button("Tab", .bytes([0x09]), "tab"),
        Button("Ctrl", .controlToggle, "hold Ctrl for the next key you type"),
        Button("^C", .bytes([0x03]), "interrupt the foreground process"),
        Button("^D", .bytes([0x04]), "end of input; exits a shell"),
        Button("^Z", .bytes([0x1a]), "suspend the foreground process"),
        Button("^L", .bytes([0x0c]), "redraw"),
        Button("^U", .bytes([0x15]), "clear the line"),
        Button("^R", .bytes([0x12]), "search history"),
        Button("↑", .arrow(.up), "up"),
        Button("↓", .arrow(.down), "down"),
        Button("←", .arrow(.left), "left"),
        Button("→", .arrow(.right), "right"),
        Button("q", .text("q"), "leave copy mode"),
        Button("y", .text("y"), "confirm a tmux prompt"),
        Button("|", .text("|"), "pipe"),
        Button("~", .text("~"), "tilde"),
        Button("-", .text("-"), "dash"),
        Button("/", .text("/"), "slash"),
    ]
}
