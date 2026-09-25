// krabink for iPad: note list + one live-synced note surface the keyboard
// types into and the Pencil draws on.

import KrabinkCore
import SwiftUI

@main
struct KrabinkApp: App {
    @State private var model = AppModel()
    @State private var theme = ThemeStore.shared
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            // `-spike 1` launch argument shows the iM2 PencilKit spike
            // screen instead of the real app; `-brushLab 1` the brush lab.
            // Dev tooling only: store (Release) builds ship without them.
            #if DEBUG
            if UserDefaults.standard.bool(forKey: "spike") {
                SpikeScreen()
            } else if UserDefaults.standard.bool(forKey: "brushLab") {
                BrushLabScreen()
            } else {
                mainView
            }
            #else
            mainView
            #endif
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

    private var mainView: some View {
        ContentView(model: model)
            .onOpenURL { url in
                // Scanned pairing QR / deep link: adopt server+token.
                _ = model.adoptPair(uri: url.absoluteString)
            }
            // Light or dark follows the Catppuccin flavour; the page
            // paper is the flavour's card colour on both platforms.
            .preferredColorScheme(theme.flavor.colorScheme)
            .tint(Theme.accent)
    }
}

struct ContentView: View {
    @Bindable var model: AppModel
    // Set-based selection: single tap outside edit mode still selects one
    // row (drives the detail pane); edit mode turns it into multi-select.
    @State private var selection = Set<String>()
    @State private var editMode: EditMode = .inactive
    @State private var showSettings = false
    @State private var confirmBulkDelete = false
    @State private var confirmBulkDeleteFinal = false

    var body: some View {
        NavigationSplitView {
            sidebar
        } detail: {
            detail
        }
    }

    // MARK: sidebar

    private var sidebar: some View {
        List(selection: $selection) {
            if model.notes.isEmpty {
                emptyLibrary
                    .listRowBackground(Color.clear)
                    .listRowSeparator(.hidden)
            }
            Section {
                ForEach(model.notes, id: \.id) { note in
                    NoteRow(note: note, selected: selection.contains(note.id))
                        .tag(note.id)
                        .listRowBackground(rowBackground(selected: selection.contains(note.id)))
                        .listRowSeparator(.hidden)
                        .listRowInsets(EdgeInsets(top: 2, leading: 12, bottom: 2, trailing: 12))
                        .swipeActions {
                            Button("delete", role: .destructive) {
                                selection.remove(note.id)
                                model.deleteNote(id: note.id)
                            }
                            .accessibilityIdentifier("deleteNote")
                        }
                }
            } header: {
                if !model.notes.isEmpty {
                    HStack {
                        Caption("Notes")
                        Spacer()
                        Text("\(model.notes.count)")
                            .font(.caption.monospacedDigit())
                            .foregroundStyle(Theme.muted)
                    }
                    .padding(.horizontal, 4)
                    .padding(.top, 4)
                }
            }
        }
        .listStyle(.plain)
        .scrollContentBackground(.hidden)
        .background(Theme.sidebar)
        .environment(\.editMode, $editMode)
        .navigationTitle("Krabink")
        .toolbarBackground(Theme.sidebar, for: .navigationBar)
        .toolbarBackground(.visible, for: .navigationBar)
        .toolbar { sidebarToolbar }
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
            syncFooter
        }
    }

    @ToolbarContentBuilder
    private var sidebarToolbar: some ToolbarContent {
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
                    createNote()
                } label: {
                    Image(systemName: "square.and.pencil")
                }
                .accessibilityIdentifier("newNote")
            }
        }
    }

    private func rowBackground(selected: Bool) -> some View {
        RoundedRectangle(cornerRadius: Theme.radius, style: .continuous)
            .fill(selected ? Theme.accentSoft : Color.clear)
            .padding(.horizontal, 8)
    }

    private var emptyLibrary: some View {
        VStack(spacing: 14) {
            LogoMark(size: 48)
            Text("No notes yet")
                .font(.headline)
                .foregroundStyle(Theme.text)
            Text("Notes sync live with every paired device.")
                .font(.footnote)
                .foregroundStyle(Theme.muted)
                .multilineTextAlignment(.center)
            Button {
                createNote()
            } label: {
                Label("Create a note", systemImage: "plus")
            }
            .buttonStyle(.primary)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 32)
    }

    /// Full sync status, the text UI tests read (`syncState`), with the
    /// settings button beside it, as on the desktop sidebar.
    private var syncFooter: some View {
        let tone = SyncTone(model.syncState)
        return HStack(spacing: 8) {
            StatusDot(color: tone.color)
            Text(model.syncState)
                .font(.footnote)
                .foregroundStyle(Theme.muted)
                .lineLimit(1)
                .truncationMode(.middle)
                .accessibilityIdentifier("syncState")
            Spacer(minLength: 0)
            Button {
                showSettings = true
            } label: {
                Image(systemName: "gearshape")
                    .font(.footnote.weight(.semibold))
                    .foregroundStyle(Theme.muted)
                    .frame(width: 28, height: 28)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel("settings")
            .accessibilityIdentifier("settings")
        }
        .padding(.leading, 12)
        .padding(.trailing, 4)
        .padding(.vertical, 4)
        .card(fill: Theme.surface)
        .padding(.horizontal, 12)
        .padding(.bottom, 8)
        .background(Theme.sidebar)
    }

    // MARK: detail

    @ViewBuilder
    private var detail: some View {
        if selection.count == 1, let id = selection.first, let note = model.note(for: id) {
            // Keyed by note: the canvas binds its text view and ink model
            // to one `NoteModel` when made, so a switch must make a new one
            // rather than update the old one in place.
            NoteDetail(model: model, note: note)
                .id(id)
        } else {
            emptyDetail
        }
    }

    private var emptyDetail: some View {
        VStack(spacing: 14) {
            LogoMark(size: 56)
            Text("select or create a note")
                .font(.title3.weight(.semibold))
                .foregroundStyle(Theme.text)
            Text("Pick one from the list, or start fresh.")
                .font(.footnote)
                .foregroundStyle(Theme.muted)
            Button {
                createNote()
            } label: {
                Label("New note", systemImage: "square.and.pencil")
            }
            .buttonStyle(.primary)
            .padding(.top, 4)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Theme.bg)
        .toolbarBackground(Theme.bg, for: .navigationBar)
    }

    private func createNote() {
        if let id = model.createNote() { selection = [id] }
    }

    private func bulkDelete() {
        for id in selection {
            model.deleteNote(id: id)
        }
        selection.removeAll()
        withAnimation { editMode = .inactive }
    }
}

/// One line of the note list: title, when it last changed, an accent bar
/// when selected.
private struct NoteRow: View {
    let note: NoteInfo
    let selected: Bool

    var body: some View {
        HStack(spacing: 10) {
            RoundedRectangle(cornerRadius: 2)
                .fill(selected ? Theme.accent : Color.clear)
                .frame(width: 3, height: 26)
            VStack(alignment: .leading, spacing: 3) {
                Text(note.title.isEmpty ? "untitled" : note.title)
                    .font(.body.weight(selected ? .semibold : .regular))
                    .foregroundStyle(note.title.isEmpty ? Theme.muted : Theme.text)
                    .lineLimit(1)
                Text(updated)
                    .font(.caption)
                    .foregroundStyle(Theme.muted)
            }
            Spacer(minLength: 0)
        }
        .padding(.vertical, 6)
        .padding(.horizontal, 6)
        .contentShape(Rectangle())
    }

    private var updated: String {
        guard note.updatedMs > 0 else { return "new" }
        let date = Date(timeIntervalSince1970: TimeInterval(note.updatedMs) / 1000)
        return date.formatted(.relative(presentation: .named))
    }
}

/// The selected note's page (styled markdown source with ink drawn over
/// it, or the same page as the reading view with markers hidden) on a
/// card under a title header with the live-sync dot, matching the desktop
/// layout.
private struct NoteDetail: View {
    let model: AppModel
    let note: NoteModel
    @State private var theme = ThemeStore.shared
    @State private var showBrushes = false
    @State private var preview = false

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            header
            NoteCanvas(model: note, flavor: theme.flavor, preview: preview)
                .card()
                .padding(.horizontal, Theme.pagePadding)
                .padding(.bottom, Theme.pagePadding)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Theme.bg)
        .ignoresSafeArea(.keyboard, edges: .bottom)
        .toolbarBackground(Theme.bg, for: .navigationBar)
        .toolbarBackground(.visible, for: .navigationBar)
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                Button {
                    preview.toggle()
                } label: {
                    Label(preview ? "Edit" : "Preview", systemImage: preview ? "pencil" : "eye")
                }
                .accessibilityIdentifier("previewToggle")
            }
            if #unavailable(iOS 18.0) {
                // iOS 17 has no custom picker items: library brushes come
                // from a sheet, and a pill names the active one.
                ToolbarItem(placement: .primaryAction) {
                    HStack(spacing: 8) {
                        if let active = note.ink.activeCustomBrush {
                            Text(BrushLibrary.shared.brush(id: active)?.name ?? active)
                                .font(.caption)
                                .foregroundStyle(Theme.text)
                                .padding(.horizontal, 8)
                                .padding(.vertical, 3)
                                .background(Capsule().fill(Theme.accentSoft))
                                .accessibilityIdentifier("activeBrush")
                        }
                        Button {
                            showBrushes = true
                        } label: {
                            Label("brushes", systemImage: "paintbrush.pointed")
                        }
                        .accessibilityIdentifier("brushes")
                    }
                }
            }
            ToolbarItem(placement: .primaryAction) {
                Button {
                    note.insertSketch()
                } label: {
                    Label("insert sketch", systemImage: "rectangle.and.pencil.and.ellipsis")
                }
                .accessibilityIdentifier("newSketch")
                .disabled(preview)
            }
            ToolbarItem(placement: .primaryAction) {
                Button {
                    note.ink.eraseLast()
                } label: {
                    Label("erase last", systemImage: "arrow.uturn.backward")
                }
                .accessibilityIdentifier("eraseLast")
            }
        }
        .sheet(isPresented: $showBrushes) { BrushSheet(model: note.ink) }
        .onDisappear { note.ink.pointerGone() }
    }

    private var header: some View {
        let tone = SyncTone(model.syncState)
        let title = model.notes.first { $0.id == note.id }?.title ?? ""
        return HStack(alignment: .firstTextBaseline, spacing: 12) {
            Text(title.isEmpty ? "untitled" : title)
                .font(.title2.weight(.semibold))
                .foregroundStyle(title.isEmpty ? Theme.muted : Theme.text)
                .lineLimit(1)
            Spacer()
            HStack(spacing: 6) {
                StatusDot(color: tone.color)
                Text(tone.short)
                    .font(.caption)
                    .foregroundStyle(Theme.muted)
            }
            // Live ink counters: what the UI tests and on-device checks read.
            Text(note.ink.status)
                .font(.system(size: 11, design: .monospaced))
                .foregroundStyle(Theme.muted)
                .lineLimit(1)
                .accessibilityIdentifier("sketchStatus")
            Caption(preview ? "preview" : "markdown")
        }
        .padding(.horizontal, Theme.pagePadding + 4)
        .padding(.top, 8)
    }
}
