// pendant for iPad: note list + live-synced markdown editor (iM1).

import PendantCore
import SwiftUI

@main
struct PendantApp: App {
    @State private var model = AppModel()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            // `-spike 1` launch argument shows the iM2 PencilKit spike
            // screen instead of the real app.
            if UserDefaults.standard.bool(forKey: "spike") {
                SpikeScreen()
            } else {
                ContentView(model: model)
            }
        }
        .onChange(of: scenePhase) { _, phase in
            // iOS tears down sockets on background; reconnect on foreground.
            switch phase {
            case .active: model.resume()
            case .background: model.suspend()
            default: break
            }
        }
    }
}

/// Identifiable wrapper so a sketch id can drive fullScreenCover(item:).
struct SketchRef: Identifiable {
    let id: String
}

struct ContentView: View {
    @Bindable var model: AppModel
    @State private var selection: String?
    @State private var openSketch: SketchRef?
    @State private var preview = false

    var body: some View {
        NavigationSplitView {
            List(model.notes, id: \.id, selection: $selection) { note in
                Text(note.title.isEmpty ? "untitled" : note.title)
                    .tag(note.id)
            }
            .navigationTitle("pendant")
            .toolbar {
                ToolbarItem(placement: .primaryAction) {
                    Button {
                        selection = model.createNote()
                    } label: {
                        Image(systemName: "square.and.pencil")
                    }
                    .accessibilityIdentifier("newNote")
                }
            }
            .safeAreaInset(edge: .bottom) {
                Text(model.syncState)
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                    .accessibilityIdentifier("syncState")
                    .padding(.bottom, 4)
            }
        } detail: {
            if let id = selection, let note = model.note(for: id) {
                Group {
                    if preview {
                        SketchPreview(model: note) { sketchId in
                            openSketch = SketchRef(id: sketchId)
                        }
                    } else {
                        MarkdownTextView(model: note)
                    }
                }
                .ignoresSafeArea(.keyboard, edges: .bottom)
                .toolbar {
                    ToolbarItem(placement: .primaryAction) {
                        Button {
                            preview.toggle()
                        } label: {
                            Image(systemName: preview ? "pencil" : "eye")
                        }
                        .accessibilityIdentifier("previewToggle")
                    }
                    ToolbarItem(placement: .primaryAction) {
                        Menu("sketches") {
                            ForEach(Array(note.sketchIds.enumerated()), id: \.element) {
                                index, sketchId in
                                Button("sketch \(index)") {
                                    openSketch = SketchRef(id: sketchId)
                                }
                                .accessibilityIdentifier("sketch-\(index)")
                            }
                            Button("new sketch") {
                                if let sketchId = note.createSketch() {
                                    openSketch = SketchRef(id: sketchId)
                                }
                            }
                            .accessibilityIdentifier("newSketch")
                        }
                        .accessibilityIdentifier("sketchMenu")
                    }
                }
                .fullScreenCover(item: $openSketch) { ref in
                    SketchScreen(model: note.sketch(for: ref.id)) {
                        openSketch = nil
                    }
                }
            } else {
                Text("select or create a note")
                    .foregroundStyle(.secondary)
            }
        }
    }
}
