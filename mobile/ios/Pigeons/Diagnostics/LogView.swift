import SwiftUI

/// What the Rust core logged, newest last. The only window into a TestFlight build.
struct LogView: View {
    @EnvironmentObject private var client: RoostClient

    var body: some View {
        ScrollViewReader { proxy in
            List(client.log) { line in
                Text(line.message)
                    .font(.caption2.monospaced())
                    .foregroundStyle(color(for: line.level))
                    .id(line.id)
            }
            .listStyle(.plain)
            .onChange(of: client.log.count) { _, _ in
                if let last = client.log.last {
                    proxy.scrollTo(last.id, anchor: .bottom)
                }
            }
        }
        .navigationTitle("Core log")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .topBarTrailing) {
                Button {
                    UIPasteboard.general.string = client.log.map(\.message).joined(separator: "\n")
                } label: {
                    Image(systemName: "doc.on.doc")
                }
            }
        }
    }

    private func color(for level: Int32) -> Color {
        switch level {
        case 1: .red
        case 2: .orange
        case 3: .primary
        default: .secondary
        }
    }
}
