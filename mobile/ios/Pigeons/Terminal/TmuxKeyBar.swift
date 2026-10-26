import SwiftUI

/// Two scrolling rows of keys above the keyboard: tmux actions on top,
/// process control and missing keys below.
struct TmuxKeyBar: View {
    @ObservedObject var controller: TerminalController

    var body: some View {
        VStack(spacing: 4) {
            row(TmuxKeys.tmuxRow, tint: .orange)
            row(TmuxKeys.controlRow, tint: .gray)
        }
        .padding(.vertical, 6)
        .background(Color(white: 0.08))
    }

    private func row(_ buttons: [TmuxKeys.Button], tint: Color) -> some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 6) {
                ForEach(buttons) { button in
                    KeyButton(
                        button: button,
                        tint: tint,
                        active: button.key == .controlToggle && controller.controlArmed
                    ) {
                        controller.press(button.key)
                    }
                }
            }
            .padding(.horizontal, 8)
        }
    }
}

private struct KeyButton: View {
    let button: TmuxKeys.Button
    let tint: Color
    let active: Bool
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Text(button.label)
                .font(.system(.footnote, design: .monospaced).weight(.medium))
                .padding(.horizontal, 10)
                .padding(.vertical, 7)
                .frame(minWidth: 36)
                .background(active ? tint : Color(white: 0.18), in: RoundedRectangle(cornerRadius: 7))
                .foregroundStyle(active ? Color.black : Color.white)
        }
        .buttonStyle(.plain)
        .accessibilityLabel(button.hint)
    }
}
