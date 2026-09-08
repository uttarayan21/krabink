// App-level state: owns the UniFFI Core, the note registry, and one
// NoteModel per open note. All FFI listener callbacks arrive on Rust's
// network thread and hop to the main actor before touching state.

import Foundation
import Network
import PendantCore

@Observable @MainActor
final class AppModel {
    let core: Core
    var notes: [NoteInfo] = []
    var syncState = "offline"
    private var open: [String: NoteModel] = [:]
    private var hasServer = false
    private let pathMonitor = NWPathMonitor()
    /// Suppress the very first path callback (fires with the initial state
    /// right after connect(), which would be a redundant reconnect).
    private var sawInitialPath = false

    init() {
        let dir = FileManager.default
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("pendant").path
        core = try! Core(dataDir: dir)
        notes = core.listNotes()
        core.setListener(listener: CoreEvents(model: self))

        // Server config via UserDefaults; launch arguments like
        // `-serverURL ws://… -token demo` populate these automatically.
        let defaults = UserDefaults.standard
        if let url = defaults.string(forKey: "serverURL"),
            let token = defaults.string(forKey: "token")
        {
            core.setSyncServer(url: url, token: token)
            hasServer = true
            try? core.connect()
            syncState = "connecting"
        }

        // Reconnect when a usable network path returns (Wi-Fi handoff, VPN,
        // airplane-mode off). The net task already backs off on drops; this
        // just shortcuts the wait.
        pathMonitor.pathUpdateHandler = { [weak self] path in
            guard path.status == .satisfied else { return }
            Task { @MainActor [weak self] in
                guard let self, self.hasServer else { return }
                if self.sawInitialPath { self.resume() } else { self.sawInitialPath = true }
            }
        }
        pathMonitor.start(queue: .global(qos: .utility))
    }

    /// Foreground / network-return: restart background sync.
    func resume() {
        guard hasServer else { return }
        try? core.connect()
    }

    /// Background: drop the socket so iOS doesn't kill us holding it.
    func suspend() {
        core.suspend()
    }

    func createNote() -> String? {
        guard let session = try? core.createNote(title: "untitled") else { return nil }
        notes = core.listNotes()
        let model = NoteModel(session: session)
        open[model.id] = model
        return model.id
    }

    func note(for id: String) -> NoteModel? {
        if let model = open[id] { return model }
        guard let session = try? core.openNote(id: id) else { return nil }
        let model = NoteModel(session: session)
        open[id] = model
        return model
    }
}

/// One open note: `text` mirrors the CRDT and drives the editor.
@Observable @MainActor
final class NoteModel: Identifiable {
    let session: NoteSession
    let id: String
    /// CRDT-authoritative text. Local typing updates it after the fact;
    /// remote changes update it first and the editor diffs against it.
    var text: String
    var synced = false
    var sketchIds: [String] = []
    private var sketches: [String: SketchModel] = [:]

    init(session: NoteSession) {
        self.session = session
        id = session.id()
        text = (try? session.text()) ?? ""
        sketchIds = (try? session.sketchIds()) ?? []
        session.setListener(listener: NoteEvents(model: self))
    }

    /// Forward one local edit (unicode-scalar offsets) to the CRDT.
    func localEdit(at: UInt64, del: UInt64, insert: String) {
        try? session.applyTextEdit(at: at, del: del, insert: insert)
    }

    func createSketch() -> String? {
        guard let id = try? session.createSketch() else { return nil }
        sketchIds = (try? session.sketchIds()) ?? sketchIds
        // Splice an inline markdown image ref so the sketch renders in the
        // preview and round-trips to the desktop (which rewrites the same
        // pendant://sketch/<id> URI to an SVG on export).
        let current = (try? session.text()) ?? text
        let embed = (current.isEmpty || current.hasSuffix("\n") ? "" : "\n")
            + "![sketch](pendant://sketch/\(id))\n"
        let at = UInt64(current.unicodeScalars.count)
        try? session.applyTextEdit(at: at, del: 0, insert: embed)
        text = (try? session.text()) ?? text
        return id
    }

    func sketch(for id: String) -> SketchModel {
        if let model = sketches[id] { return model }
        let model = SketchModel(session: session, sketchId: id)
        sketches[id] = model
        return model
    }

    /// Remote stroke change: refresh the sketch registry (a first stroke may
    /// reveal a sketch created remotely) and route to the open canvas.
    func remoteStrokes(sketch: String) {
        sketchIds = (try? session.sketchIds()) ?? sketchIds
        sketches[sketch]?.remoteChanged()
    }

    // Wet-ink routing: Begin carries the sketch id; Points/End only the
    // stroke id, so remember the mapping for the stroke's lifetime.
    private var wetStrokeSketch: [String: String] = [:]

    func wetBegin(sketch: String, stroke: String, tool: Tool, color: UInt32, baseWidth: Float) {
        wetStrokeSketch[stroke] = sketch
        sketches[sketch]?.remoteWetBegin(
            stroke: stroke, tool: tool, color: color, baseWidth: baseWidth)
    }

    func wetPoints(stroke: String, points: [WetPoint]) {
        guard let sketch = wetStrokeSketch[stroke] else { return }
        sketches[sketch]?.remoteWetPoints(stroke: stroke, points: points)
    }

    func wetEnd(stroke: String) {
        guard let sketch = wetStrokeSketch.removeValue(forKey: stroke) else { return }
        sketches[sketch]?.remoteWetEnd(stroke: stroke)
    }
}

// Nonisolated bridges: uniffi calls these from the Rust network thread.

private final class CoreEvents: CoreListener {
    private weak var model: AppModel?
    init(model: AppModel) { self.model = model }

    func notesChanged(notes: [NoteInfo]) {
        Task { @MainActor [weak model] in model?.notes = notes }
    }

    func syncState(state: SyncState) {
        let label: String
        switch state {
        case .disconnected: label = "offline"
        case .connecting: label = "connecting"
        case .connected: label = "connected"
        case .fatal(let message): label = "error: \(message)"
        }
        Task { @MainActor [weak model] in model?.syncState = label }
    }
}

private final class NoteEvents: NoteListener {
    private weak var model: NoteModel?
    init(model: NoteModel) { self.model = model }

    func synced() {
        Task { @MainActor [weak model] in model?.synced = true }
    }

    func textChanged(text: String) {
        // Don't trust the payload: it may predate a local keystroke applied
        // since. The doc merges both, and local splices happen on the main
        // actor before the view applies them, so this read is never stale.
        Task { @MainActor [weak model] in
            guard let model else { return }
            model.text = (try? model.session.text()) ?? text
        }
    }

    func strokesChanged(sketch: String) {
        Task { @MainActor [weak model] in model?.remoteStrokes(sketch: sketch) }
    }
    func wetBegin(sketch: String, stroke: String, tool: Tool, color: UInt32, baseWidth: Float) {
        Task { @MainActor [weak model] in
            model?.wetBegin(
                sketch: sketch, stroke: stroke, tool: tool, color: color, baseWidth: baseWidth)
        }
    }

    func wetPoints(stroke: String, sentMs: UInt64, points: [WetPoint]) {
        Task { @MainActor [weak model] in
            model?.wetPoints(stroke: stroke, points: points)
        }
    }

    func wetEnd(stroke: String) {
        Task { @MainActor [weak model] in model?.wetEnd(stroke: stroke) }
    }
}
