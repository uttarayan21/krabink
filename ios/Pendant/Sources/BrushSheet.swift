// iOS 17 has no custom tool-picker items, so library brushes are offered
// from a sheet instead: pick one and the pen draws with it at the colour
// and width the PencilKit picker last reported; picking any PencilKit
// tool switches back. A pill in the sketch toolbar names the active
// custom brush.

import PendantCore
import SwiftUI

struct BrushSheet: View {
    let model: SketchModel
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            List(BrushLibrary.shared.brushes, id: \.id) { brush in
                Button {
                    model.pick(custom: brush)
                    dismiss()
                } label: {
                    HStack {
                        Image(uiImage: BrushIcon.image(brush: brush, color: .label, width: 8))
                            .resizable()
                            .scaledToFit()
                            .frame(width: 32, height: 48)
                        Text(brush.name)
                        Spacer()
                        if model.activeCustomBrush == brush.id {
                            Image(systemName: "checkmark")
                        }
                    }
                }
                .accessibilityIdentifier("brush-\(brush.id)")
            }
            .navigationTitle("Brushes")
            .toolbar {
                Button("Done") { dismiss() }
            }
        }
    }
}
