import SwiftUI
import UIKit

/// A read-only, selectable text view for file contents. UITextView rather
/// than SwiftUI `Text` so large files scroll smoothly and text can be
/// selected and copied.
struct CodeTextView: UIViewRepresentable {
    let text: NSAttributedString

    func makeUIView(context: Context) -> UITextView {
        let view = UITextView()
        view.isEditable = false
        view.isSelectable = true
        view.isScrollEnabled = true
        view.alwaysBounceVertical = true
        view.backgroundColor = CodeHighlighter.background
        view.indicatorStyle = .white
        view.textContainerInset = UIEdgeInsets(top: 12, left: 8, bottom: 24, right: 8)
        view.textContainer.lineFragmentPadding = 0
        view.dataDetectorTypes = []
        view.keyboardAppearance = .dark
        view.attributedText = text
        context.coordinator.shown = text
        return view
    }

    func updateUIView(_ view: UITextView, context: Context) {
        // UITextView copies what it is given, so compare against what we set.
        if context.coordinator.shown !== text {
            view.attributedText = text
            context.coordinator.shown = text
        }
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    final class Coordinator {
        var shown: NSAttributedString?
    }
}
