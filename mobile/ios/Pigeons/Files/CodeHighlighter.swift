import Foundation
import Highlightr
import UIKit

/// Turns file text into a monospaced, dark-themed attributed string.
///
/// Markdown gets a custom pass (see [`MarkdownHighlighter`]) because it is what
/// gets read most on a phone and highlight.js's markdown grammar is thin.
/// Everything else with a known extension goes through Highlightr, which runs
/// highlight.js in JavaScriptCore. Highlighting runs on a private serial
/// queue because Highlightr is not thread safe and large files take a while.
enum CodeHighlighter {
    /// atom-one-dark's background, used for every viewer so plain files match.
    static let background = UIColor(red: 0x28 / 255, green: 0x2c / 255, blue: 0x34 / 255, alpha: 1)
    static let foreground = UIColor(red: 0xab / 255, green: 0xb2 / 255, blue: 0xbf / 255, alpha: 1)

    /// Above this many characters a file is shown plain; highlight.js would
    /// take seconds and the result is unreadable at that size anyway.
    static let maxHighlightLength = 200_000

    private static let queue = DispatchQueue(label: "computer.n0.pigeons.highlight", qos: .userInitiated)
    private static var highlightr: Highlightr?

    /// highlight.js language for a file name, or nil for plain text.
    static func language(for filename: String) -> String? {
        let name = filename.lowercased()
        switch (name as NSString).pathExtension {
        case "rs": return "rust"
        case "js", "mjs", "cjs", "jsx": return "javascript"
        case "ts", "tsx", "mts", "cts": return "typescript"
        case "yaml", "yml": return "yaml"
        case "toml": return "ini"
        case "json", "jsonc": return "json"
        case "swift": return "swift"
        case "sh", "bash", "zsh": return "bash"
        case "py": return "python"
        case "go": return "go"
        case "c", "h": return "c"
        case "cpp", "cc", "hpp": return "cpp"
        case "rb": return "ruby"
        case "html", "htm": return "xml"
        case "css": return "css"
        case "sql": return "sql"
        case "proto": return "protobuf"
        case "diff", "patch": return "diff"
        case "nix": return "nix"
        default:
            switch name {
            case "dockerfile": return "dockerfile"
            case "makefile": return "makefile"
            case "cargo.lock": return "ini"
            default: return nil
            }
        }
    }

    static func isMarkdown(_ filename: String) -> Bool {
        ["md", "markdown", "mdx"].contains((filename.lowercased() as NSString).pathExtension)
    }

    /// Highlight `text` for `filename`. Always returns something renderable.
    static func highlight(_ text: String, filename: String, fontSize: CGFloat) async -> NSAttributedString {
        await withCheckedContinuation { continuation in
            queue.async {
                continuation.resume(returning: render(text, filename: filename, fontSize: fontSize))
            }
        }
    }

    // MARK: Queue-confined

    private static func render(_ text: String, filename: String, fontSize: CGFloat) -> NSAttributedString {
        let font = UIFont.monospacedSystemFont(ofSize: fontSize, weight: .regular)
        if text.count > maxHighlightLength {
            return plain(text, font: font)
        }
        if isMarkdown(filename) {
            return MarkdownHighlighter(fontSize: fontSize, code: { body, lang in
                Self.code(body, lang, fontSize: fontSize)
            }).render(text)
        }
        if let lang = language(for: filename), let highlighted = code(text, lang, fontSize: fontSize) {
            return highlighted
        }
        return plain(text, font: font)
    }

    private static func code(_ text: String, _ lang: String, fontSize: CGFloat) -> NSAttributedString? {
        guard let highlightr = engine(fontSize: fontSize) else { return nil }
        return highlightr.highlight(text, as: lang, fastRender: true)
    }

    private static func engine(fontSize: CGFloat) -> Highlightr? {
        if highlightr == nil {
            highlightr = Highlightr()
            _ = highlightr?.setTheme(to: "atom-one-dark")
        }
        guard let highlightr else { return nil }
        if highlightr.theme.codeFont.pointSize != fontSize {
            highlightr.theme.setCodeFont(UIFont.monospacedSystemFont(ofSize: fontSize, weight: .regular))
        }
        return highlightr
    }

    static func plain(_ text: String, font: UIFont) -> NSAttributedString {
        NSAttributedString(string: text, attributes: [.font: font, .foregroundColor: foreground])
    }
}

/// Line-oriented markdown colouring that never changes glyph widths: the same
/// monospaced family throughout, bold for headings and strong text, colour
/// for everything else. Fenced code blocks with a language go through the
/// code highlighter.
struct MarkdownHighlighter {
    let fontSize: CGFloat
    /// Highlights a fenced block's code in `lang`; nil falls back to plain green.
    let code: (_ text: String, _ lang: String) -> NSAttributedString?

    // atom-one-dark palette
    private static let red = UIColor(red: 0xe0 / 255, green: 0x6c / 255, blue: 0x75 / 255, alpha: 1)
    private static let orange = UIColor(red: 0xd1 / 255, green: 0x9a / 255, blue: 0x66 / 255, alpha: 1)
    private static let yellow = UIColor(red: 0xe5 / 255, green: 0xc0 / 255, blue: 0x7b / 255, alpha: 1)
    private static let green = UIColor(red: 0x98 / 255, green: 0xc3 / 255, blue: 0x79 / 255, alpha: 1)
    private static let cyan = UIColor(red: 0x56 / 255, green: 0xb6 / 255, blue: 0xc2 / 255, alpha: 1)
    private static let blue = UIColor(red: 0x61 / 255, green: 0xaf / 255, blue: 0xef / 255, alpha: 1)
    private static let purple = UIColor(red: 0xc6 / 255, green: 0x78 / 255, blue: 0xdd / 255, alpha: 1)
    private static let dim = UIColor(red: 0x5c / 255, green: 0x63 / 255, blue: 0x70 / 255, alpha: 1)
    private static let bright = UIColor(white: 0.93, alpha: 1)
    private static let headingColors = [red, orange, yellow, green, cyan, blue]

    private static let fence = try! NSRegularExpression(pattern: #"^\s*(```|~~~)\s*([A-Za-z0-9_+-]*)"#)
    private static let heading = try! NSRegularExpression(pattern: #"^(#{1,6})\s+"#)
    private static let rule = try! NSRegularExpression(pattern: #"^\s*([-*_])(\s*\1){2,}\s*$"#)
    private static let listMarker = try! NSRegularExpression(pattern: #"^\s*(?:[-*+]|\d+[.)])\s"#)
    private static let quote = try! NSRegularExpression(pattern: #"^\s*(>\s?)+"#)
    private static let codeSpan = try! NSRegularExpression(pattern: #"`[^`\n]+`"#)
    private static let strong = try! NSRegularExpression(pattern: #"\*\*[^*\n]+\*\*|__[^_\n]+__"#)
    private static let emphasis = try! NSRegularExpression(pattern: #"(?<![*\w])\*[^*\n]+\*(?!\*)|(?<![_\w])_[^_\n]+_(?!\w)"#)
    private static let link = try! NSRegularExpression(pattern: #"!?\[([^\]\n]*)\](\([^)\n]*\))"#)
    private static let url = try! NSRegularExpression(pattern: #"https?://[^\s)>\]]+"#)
    private static let tag = try! NSRegularExpression(pattern: #"</?[A-Za-z][^>\n]*>"#)
    private static let pipe = try! NSRegularExpression(pattern: #"\|"#)

    func render(_ text: String) -> NSAttributedString {
        let regular = UIFont.monospacedSystemFont(ofSize: fontSize, weight: .regular)
        let bold = UIFont.monospacedSystemFont(ofSize: fontSize, weight: .bold)
        let out = NSMutableAttributedString()

        var fenceLang: String?
        var fenceLines: [String] = []

        func flushFence() {
            let body = fenceLines.joined(separator: "\n") + (fenceLines.isEmpty ? "" : "\n")
            if let lang = fenceLang, !lang.isEmpty, let highlighted = code(body, lang) {
                out.append(highlighted)
            } else {
                out.append(NSAttributedString(string: body, attributes: [.font: regular, .foregroundColor: Self.green]))
            }
            fenceLines.removeAll()
        }

        for line in text.components(separatedBy: "\n") {
            let ns = line as NSString
            let whole = NSRange(location: 0, length: ns.length)

            if let m = Self.fence.firstMatch(in: line, range: whole) {
                if fenceLang != nil {
                    flushFence()
                    fenceLang = nil
                } else {
                    fenceLang = ns.substring(with: m.range(at: 2))
                }
                out.append(NSAttributedString(string: line + "\n", attributes: [.font: regular, .foregroundColor: Self.dim]))
                continue
            }
            if fenceLang != nil {
                fenceLines.append(line)
                continue
            }

            let attributed = NSMutableAttributedString(string: line + "\n", attributes: [.font: regular, .foregroundColor: CodeHighlighter.foreground])

            if let m = Self.heading.firstMatch(in: line, range: whole) {
                let level = m.range(at: 1).length
                attributed.addAttributes([.font: bold, .foregroundColor: Self.headingColors[min(level, 6) - 1]], range: whole)
                attributed.addAttribute(.foregroundColor, value: Self.dim, range: m.range(at: 1))
                out.append(attributed)
                continue
            }
            if Self.rule.firstMatch(in: line, range: whole) != nil {
                attributed.addAttribute(.foregroundColor, value: Self.dim, range: whole)
                out.append(attributed)
                continue
            }
            if let m = Self.quote.firstMatch(in: line, range: whole) {
                attributed.addAttribute(.foregroundColor, value: Self.green, range: whole)
                attributed.addAttribute(.foregroundColor, value: Self.dim, range: m.range)
            }
            if let m = Self.listMarker.firstMatch(in: line, range: whole) {
                attributed.addAttribute(.foregroundColor, value: Self.blue, range: m.range)
            }
            for m in Self.pipe.matches(in: line, range: whole) {
                attributed.addAttribute(.foregroundColor, value: Self.dim, range: m.range)
            }
            for m in Self.tag.matches(in: line, range: whole) {
                attributed.addAttribute(.foregroundColor, value: Self.dim, range: m.range)
            }
            for m in Self.strong.matches(in: line, range: whole) {
                attributed.addAttributes([.font: bold, .foregroundColor: Self.bright], range: m.range)
            }
            for m in Self.emphasis.matches(in: line, range: whole) {
                attributed.addAttribute(.foregroundColor, value: Self.yellow, range: m.range)
            }
            for m in Self.link.matches(in: line, range: whole) {
                attributed.addAttributes([.foregroundColor: Self.blue, .underlineStyle: NSUnderlineStyle.single.rawValue], range: m.range(at: 1))
                attributed.addAttribute(.foregroundColor, value: Self.dim, range: m.range(at: 2))
            }
            for m in Self.url.matches(in: line, range: whole) {
                attributed.addAttributes([.foregroundColor: Self.blue, .underlineStyle: NSUnderlineStyle.single.rawValue], range: m.range)
            }
            // Code spans last so their colour wins over anything inside them.
            for m in Self.codeSpan.matches(in: line, range: whole) {
                attributed.addAttributes([.font: regular, .foregroundColor: Self.cyan, .underlineStyle: 0], range: m.range)
            }
            out.append(attributed)
        }
        if fenceLang != nil {
            flushFence()
        }
        return out
    }
}
