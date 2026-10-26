import SwiftUI

/// One file from the roost, highlighted when its type is known.
struct FileViewerScreen: View {
    let route: FileRoute
    @EnvironmentObject private var client: RoostClient
    @EnvironmentObject private var store: SettingsStore

    @State private var content: FileContent?
    @State private var rendered: NSAttributedString?
    @State private var error: String?

    /// Enough for any source file; bigger than this is not something to read on a phone.
    private let maxBytes: UInt64 = 1 << 20

    var body: some View {
        Group {
            if let error {
                ContentUnavailableView("Could not open file", systemImage: "doc.badge.ellipsis", description: Text(error))
            } else if let content, content.binary {
                ContentUnavailableView(
                    "Binary file",
                    systemImage: "doc.zipper",
                    description: Text("\(content.size.byteCountLabel). Nothing to show as text.")
                )
            } else if let rendered {
                VStack(spacing: 0) {
                    if let content, content.truncated {
                        Text("Showing the first \(maxBytes.byteCountLabel) of \(content.size.byteCountLabel)")
                            .font(.caption)
                            .foregroundStyle(.black)
                            .frame(maxWidth: .infinity)
                            .padding(.vertical, 4)
                            .background(Color.orange)
                    }
                    CodeTextView(text: rendered)
                }
            } else {
                ProgressView("Loading…")
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color(uiColor: CodeHighlighter.background).ignoresSafeArea())
        .navigationTitle(route.name)
        .navigationBarTitleDisplayMode(.inline)
        .toolbarBackground(Color(white: 0.08), for: .navigationBar)
        .toolbarColorScheme(.dark, for: .navigationBar)
        .toolbar {
            ToolbarItem(placement: .topBarTrailing) {
                Button {
                    UIPasteboard.general.string = content?.text
                } label: {
                    Image(systemName: "doc.on.doc")
                }
                .disabled(content == nil || content?.binary == true)
                .accessibilityLabel("Copy contents")
            }
        }
        .task(id: route) { await load() }
    }

    private func load() async {
        do {
            let file = try await client.readFile(route.path, maxBytes: maxBytes)
            content = file
            guard !file.binary else { return }
            rendered = await CodeHighlighter.highlight(file.text, filename: route.name, fontSize: CGFloat(store.settings.fontSize))
        } catch {
            self.error = error.localizedDescription
        }
    }
}
