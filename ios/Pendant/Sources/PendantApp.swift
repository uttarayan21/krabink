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
            // screen instead of the real app; `-brushLab 1` the brush lab.
            if UserDefaults.standard.bool(forKey: "spike") {
                SpikeScreen()
            } else if UserDefaults.standard.bool(forKey: "brushLab") {
                BrushLabScreen()
            } else {
                ContentView(model: model)
                    .onOpenURL { url in
                        // Scanned pairing QR / deep link: adopt server+token.
                        _ = model.adoptPair(uri: url.absoluteString)
                    }
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
    // Set-based selection: single tap outside edit mode still selects one
    // row (drives the detail pane); edit mode turns it into multi-select.
    @State private var selection = Set<String>()
    @State private var editMode: EditMode = .inactive
    @State private var openSketch: SketchRef?
    @State private var preview = false
    @State private var showSettings = false
    @State private var confirmBulkDelete = false
    @State private var confirmBulkDeleteFinal = false

    var body: some View {
        NavigationSplitView {
            List(selection: $selection) {
                ForEach(model.notes, id: \.id) { note in
                    Text(note.title.isEmpty ? "untitled" : note.title)
                        .tag(note.id)
                        .swipeActions {
                            Button("delete", role: .destructive) {
                                selection.remove(note.id)
                                model.deleteNote(id: note.id)
                            }
                            .accessibilityIdentifier("deleteNote")
                        }
                }
            }
            .environment(\.editMode, $editMode)
            .navigationTitle("pendant")
            .toolbar {
                if editMode.isEditing {
                    ToolbarItem(placement: .cancellationAction) {
                        Button("done") {
                            withAnimation {
                                selection.removeAll()
                                editMode = .inactive
                            }
                        }
                        .accessibilityIdentifier("doneSelecting")
                    }
                    ToolbarItem(placement: .primaryAction) {
                        Button("delete (\(selection.count))", role: .destructive) {
                            confirmBulkDelete = true
                        }
                        .disabled(selection.isEmpty)
                        .accessibilityIdentifier("bulkDelete")
                    }
                } else {
                    ToolbarItem(placement: .topBarLeading) {
                        Button {
                            withAnimation {
                                selection.removeAll()
                                editMode = .active
                            }
                        } label: {
                            Image(systemName: "checkmark.circle")
                        }
                        .accessibilityIdentifier("selectNotes")
                    }
                    ToolbarItem(placement: .primaryAction) {
                        Button {
                            if let id = model.createNote() { selection = [id] }
                        } label: {
                            Image(systemName: "square.and.pencil")
                        }
                        .accessibilityIdentifier("newNote")
                    }
                    ToolbarItem(placement: .topBarTrailing) {
                        Button {
                            showSettings = true
                        } label: {
                            Image(systemName: "gearshape")
                        }
                        .accessibilityIdentifier("settings")
                    }
                }
            }
            .alert(
                "delete \(selection.count) note\(selection.count == 1 ? "" : "s")?",
                isPresented: $confirmBulkDelete
            ) {
                Button("delete", role: .destructive) {
                    confirmBulkDeleteFinal = true
                }
                Button("cancel", role: .cancel) {}
            } message: {
                Text("you will be asked to confirm once more.")
            }
            .alert(
                "really delete \(selection.count) note\(selection.count == 1 ? "" : "s")?",
                isPresented: $confirmBulkDeleteFinal
            ) {
                Button("delete forever", role: .destructive) {
                    bulkDelete()
                }
                Button("cancel", role: .cancel) {}
            } message: {
                Text("this cannot be undone.")
            }
            .sheet(isPresented: $showSettings) {
                SettingsScreen(model: model)
            }
            .safeAreaInset(edge: .bottom) {
                Text(model.syncState)
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                    .accessibilityIdentifier("syncState")
                    .padding(.bottom, 4)
            }
        } detail: {
            if selection.count == 1, let id = selection.first, let note = model.note(for: id) {
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

    private func bulkDelete() {
        for id in selection {
            model.deleteNote(id: id)
        }
        selection.removeAll()
        withAnimation { editMode = .inactive }
    }
}
