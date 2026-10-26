import SwiftTerm
import UIKit

/// SwiftTerm's terminal view, tuned for driving tmux from a phone.
///
/// Scrolling: SwiftTerm already turns a one-finger drag into mouse wheel
/// reports whenever the application has mouse tracking on, and into cursor
/// keys when an alternate-screen application asks for that instead. The core
/// turns tmux's mouse mode on when it attaches, so a drag scrolls a pane's
/// history through tmux's copy mode, which is what we want. Two fingers still
/// scroll SwiftTerm's own scrollback.
///
/// Ctrl: the soft keyboard has no Ctrl key. The key bar arms one for the next
/// character typed, and `insertText` turns that character into its control
/// byte.
final class TmuxTerminalView: TerminalView {
    /// Set by the key bar; cleared after one character.
    var controlArmed = false {
        didSet { onControlArmedChanged?(controlArmed) }
    }
    var onControlArmedChanged: ((Bool) -> Void)?

    override init(frame: CGRect) {
        super.init(frame: frame)
        configure()
    }

    required init?(coder: NSCoder) {
        super.init(coder: coder)
        configure()
    }

    private func configure() {
        // The app draws its own key bar in SwiftUI; drop SwiftTerm's accessory.
        inputAccessoryView = nil
        allowMouseReporting = true
        keyboardAppearance = .dark
        nativeBackgroundColor = .black
        nativeForegroundColor = UIColor(white: 0.92, alpha: 1)
        backgroundColor = .black
        keyboardDismissMode = .interactive
    }

    override func insertText(_ text: String) {
        if controlArmed {
            controlArmed = false
            if text.count == 1, let ch = text.first, let byte = TmuxKeys.control(ch) {
                send([byte])
                return
            }
        }
        super.insertText(text)
    }

    /// Send one key-bar key. Arrow keys go through SwiftTerm so they respect
    /// the terminal's cursor-key mode.
    func press(_ key: TmuxKeys.Key, prefix: [UInt8]) {
        switch key {
        case .bytes(let bytes):
            send(bytes)
        case .text(let text):
            send(txt: text)
        case .prefixed(let suffix):
            send(prefix + Array(suffix.utf8))
        case .controlToggle:
            controlArmed.toggle()
        case .arrow(.up):
            sendKeyUp()
        case .arrow(.down):
            sendKeyDown()
        case .arrow(.left):
            sendKeyLeft()
        case .arrow(.right):
            sendKeyRight()
        }
    }
}
