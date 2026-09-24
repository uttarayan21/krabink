// iOS 17 has no custom tool-picker items, so library brushes are offered
// from a sheet instead: pick one and the pen draws with it at the colour
// and width the PencilKit picker last reported; picking any PencilKit
// tool switches back. A pill in the note toolbar names the active
// custom brush.

import KrabinkCore
import SwiftUI

struct BrushSheet: View {
    let model: PageInkModel
    @State private var theme = ThemeStore.shared
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            List(BrushLibrary.shared.brushes, id: \.id) { brush in
                Button {
                    model.pick(custom: brush)
                    dismiss()
                } label: {
                    HStack(spacing: 12) {
                        Image(uiImage: BrushIcon.image(brush: brush, color: .themeText, width: 8))
                            .resizable()
                            .scaledToFit()
                            .frame(width: 32, height: 48)
                        Text(brush.name)
                            .foregroundStyle(Theme.text)
                        Spacer()
                        if model.activeCustomBrush == brush.id {
                            Image(systemName: "checkmark")
                                .foregroundStyle(Theme.accent)
                        }
                    }
                }
                .listRowBackground(Theme.surface)
                .accessibilityIdentifier("brush-\(brush.id)")
            }
            .scrollContentBackground(.hidden)
            .background(Theme.bg)
            .navigationTitle("Brushes")
            .toolbar {
                Button("Done") { dismiss() }
            }
        }
        .preferredColorScheme(theme.flavor.colorScheme)
        .tint(Theme.accent)
    }
}
