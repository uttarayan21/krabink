// App-level state: owns the UniFFI Core, the note registry, and one
// NoteModel per open note. All FFI listener callbacks arrive on Rust's
// network thread and hop to the main actor before touching state.

import Foundation
import Network
import PendantCore
import UIKit

@Observable @MainActor
final class AppModel {
    let core: Core
    var notes: [NoteInfo] = []
    var syncState = "offline"
    private var target: PairInfo?
    private var discovery: RelayDiscovery?
    private var graceTask: Task<Void, Never>?
    private var graceOver = false
    /// Last direct list handed to the core, to skip no-op reconnects.
    private var applied: [String]?
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
        BrushLibrary.shared.attach(core)

        // Server config via UserDefaults; launch arguments like
        // `-serverURL ws://… -token demo` populate these automatically.
        let defaults = UserDefaults.standard
        if let url = defaults.string(forKey: "serverURL"),
            let token = defaults.string(forKey: "token")
        {
            applyTarget(
                PairInfo(
                    server: url, token: token,
                    fallback: defaults.string(forKey: "fallbackURL"),
                    alt: defaults.stringArray(forKey: "altURLs") ?? [],
                    relayId: defaults.string(forKey: "relayId")))
        }
        // `-pairURI pendant://pair?…` takes the same path as a scanned QR;
        // exists so UI tests can exercise pairing without a camera.
        if let uri = defaults.string(forKey: "pairURI") {
            _ = adoptPair(uri: uri)
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

    /// Adopt sync coordinates from a `pendant://pair` URI (QR scan, deep
    /// link, or `-pairURI` launch argument). Persists them so the next
    /// launch reconnects without re-pairing.
    func adoptPair(uri: String) -> Bool {
        guard let info = parsePairUri(uri: uri) else { return false }
        let defaults = UserDefaults.standard
        defaults.set(info.server, forKey: "serverURL")
        defaults.set(info.token, forKey: "token")
        defaults.set(info.fallback, forKey: "fallbackURL")
        defaults.set(info.alt, forKey: "altURLs")
        defaults.set(info.relayId, forKey: "relayId")
        applyTarget(info)
        return true
    }

    /// The stored pairing coordinates; nil while offline.
    var pairInfo: PairInfo? {
        let defaults = UserDefaults.standard
        guard let url = defaults.string(forKey: "serverURL"),
            let token = defaults.string(forKey: "token")
        else { return nil }
        return PairInfo(
            server: url, token: token,
            fallback: defaults.string(forKey: "fallbackURL"),
            alt: defaults.stringArray(forKey: "altURLs") ?? [],
            relayId: defaults.string(forKey: "relayId"))
    }

    /// The URI this device shows as a QR code; nil while offline.
    var pairURI: String? {
        pairInfo.map(buildPairUri)
    }

    /// Direct URL Bonjour found for the paired desktop, if any.
    var discoveredURL: String? { discovery?.url }

    /// Adopt pairing coordinates: start looking for the desktop on the
    /// local network, then hand the core its candidate list (see
    /// `retarget`). Registration in the device registry rides along.
    private func applyTarget(_ info: PairInfo) {
        target = info
        hasServer = true
        applied = nil
        graceOver = false
        discovery?.stop()
        discovery = nil
        graceTask?.cancel()
        if let relayId = info.relayId {
            let finder = RelayDiscovery(relayId: relayId)
            finder.onChange = { [weak self] in self?.retarget() }
            finder.start()
            discovery = finder
            syncState = "looking for desktop…"
            // Give Bonjour a moment before committing to the stored
            // addresses; a hit cancels the wait via onChange.
            graceTask = Task { [weak self] in
                try? await Task.sleep(for: .seconds(2))
                guard !Task.isCancelled else { return }
                self?.graceOver = true
                self?.retarget()
            }
        } else {
            graceOver = true
            retarget()
        }
        // Announce this device in the synced registry so peers can list it.
        try? core.registerDevice(
            name: UIDevice.current.name, platform: UIDevice.current.model)
    }

    /// Hand the core its direct candidates (Bonjour hit first, then the
    /// QR's addresses) plus the fallback relay. The core races the direct
    /// paths and only uses the fallback when none answers.
    private func retarget() {
        guard let target, graceOver || discovery?.url != nil else { return }
        graceOver = true
        var direct = [target.server] + target.alt
        if let found = discovery?.url {
            direct.removeAll { $0 == found }
            direct.insert(found, at: 0)
        }
        guard direct != applied else { return }
        applied = direct
        core.setSyncServer(direct: direct, token: target.token, fallback: target.fallback)
        try? core.connect()
        syncState = "connecting"
    }

    /// Registry row is gone everywhere once this syncs; note history stays
    /// in local stores (GC is backlog).
    func deleteNote(id: String) {
        try? core.deleteNote(id: id)
        open[id] = nil
        notes = core.listNotes()
    }

    func devices() -> [DeviceInfo] {
        core.listDevices()
    }

    /// Forget a paired device everywhere. It comes back if it reconnects
    /// with the same token; this is housekeeping, not revocation.
    func removeDevice(id: String) {
        try? core.removeDevice(id: id)
    }

    /// Foreground / network-return: restart background sync.
    func resume() {
        guard hasServer else { return }
        discovery?.start()
        try? core.connect()
    }

    /// Background: drop the socket so iOS doesn't kill us holding it.
    func suspend() {
        discovery?.stop()
        core.suspend()
    }

    func createNote() -> String? {
        guard let session = try? core.createNote(title: "untitled") else { return nil }
        notes = core.listNotes()
        let model = adopt(NoteModel(session: session))
        return model.id
    }

    func note(for id: String) -> NoteModel? {
        if let model = open[id] { return model }
        guard let session = try? core.openNote(id: id) else { return nil }
        return adopt(NoteModel(session: session))
    }

    /// A local `setTitle` doesn't emit `notesChanged` (only network
    /// workspace updates do), so the note list refreshes here.
    private func adopt(_ model: NoteModel) -> NoteModel {
        model.onTitleChanged = { [weak self] in
            guard let self else { return }
            notes = core.listNotes()
        }
        open[model.id] = model
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
    var onTitleChanged: (() -> Void)?
    private var sketches: [String: SketchModel] = [:]
    private var titleTask: Task<Void, Never>?

    init(session: NoteSession) {
        self.session = session
        id = session.id()
        text = (try? session.text()) ?? ""
        sketchIds = (try? session.sketchIds()) ?? []
        session.setListener(listener: NoteEvents(model: self))
        backfillEmbeds()
        scheduleTitleSync()
    }

    /// Forward one local edit (unicode-scalar offsets) to the CRDT.
    func localEdit(at: UInt64, del: UInt64, insert: String) {
        try? session.applyTextEdit(at: at, del: del, insert: insert)
        scheduleTitleSync()
    }

    // MARK: title derivation

    /// Sidebar title mirrors the first markdown heading (or first non-blank
    /// line) of the note.
    static func derivedTitle(_ text: String) -> String {
        for line in text.split(separator: "\n") {
            let stripped = line.drop(while: { $0 == "#" })
                .trimmingCharacters(in: .whitespaces)
            if stripped.isEmpty { continue }
            return String(stripped.prefix(64))
        }
        return "untitled"
    }

    /// Debounced: typing on the heading line would otherwise commit the
    /// workspace doc on every keystroke.
    func scheduleTitleSync() {
        titleTask?.cancel()
        titleTask = Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(400))
            guard !Task.isCancelled else { return }
            self?.syncTitle()
        }
    }

    private func syncTitle() {
        let derived = Self.derivedTitle((try? session.text()) ?? text)
        let current = (try? session.title()) ?? nil
        guard derived != current else { return }
        try? session.setTitle(title: derived)
        onTitleChanged?()
    }

    /// Splice an embed ref for any sketch the text doesn't mention — notes
    /// from before embeds existed, or sketches a peer created without one.
    private func backfillEmbeds() {
        var current = (try? session.text()) ?? text
        for sketchId in sketchIds where !current.contains("pendant://sketch/\(sketchId)") {
            let embed = (current.isEmpty || current.hasSuffix("\n") ? "" : "\n")
                + "![sketch](pendant://sketch/\(sketchId))\n"
            try? session.applyTextEdit(
                at: UInt64(current.unicodeScalars.count), del: 0, insert: embed)
            current += embed
        }
        text = (try? session.text()) ?? text
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

    func wetBegin(
        sketch: String, stroke: String, tool: Tool, color: UInt32, baseWidth: Float, spec: Data?
    ) {
        wetStrokeSketch[stroke] = sketch
        sketches[sketch]?.remoteWetBegin(
            stroke: stroke, tool: tool, color: color, baseWidth: baseWidth, spec: spec)
    }

    func wetPoints(stroke: String, points: [StrokePoint]) {
        guard let sketch = wetStrokeSketch[stroke] else { return }
        sketches[sketch]?.remoteWetPoints(stroke: stroke, points: points)
    }

    func wetEnd(stroke: String) {
        guard let sketch = wetStrokeSketch.removeValue(forKey: stroke) else { return }
        sketches[sketch]?.remoteWetEnd(stroke: stroke)
    }

    func wetCancel(stroke: String) {
        guard let sketch = wetStrokeSketch.removeValue(forKey: stroke) else { return }
        sketches[sketch]?.remoteWetCancel(stroke: stroke)
    }
}

// Nonisolated bridges: uniffi calls these from the Rust network thread.

private final class CoreEvents: CoreListener {
    private weak var model: AppModel?
    init(model: AppModel) { self.model = model }

    func notesChanged(notes: [NoteInfo]) {
        Task { @MainActor [weak model] in model?.notes = notes }
    }

    func brushesChanged(brushes: [BrushInfo]) {
        Task { @MainActor in BrushLibrary.shared.setShared(brushes) }
    }

    func syncState(state: SyncState) {
        let label: String
        switch state {
        case .disconnected: label = "offline"
        case .connecting: label = "connecting"
        case .connected(let url): label = "connected via \(url)"
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
            model.scheduleTitleSync()
        }
    }

    func strokesChanged(sketch: String) {
        Task { @MainActor [weak model] in model?.remoteStrokes(sketch: sketch) }
    }
    func wetBegin(
        sketch: String, stroke: String, tool: Tool, color: UInt32, baseWidth: Float, spec: Data?
    ) {
        Task { @MainActor [weak model] in
            model?.wetBegin(
                sketch: sketch, stroke: stroke, tool: tool, color: color, baseWidth: baseWidth,
                spec: spec)
        }
    }

    func wetPoints(stroke: String, sentMs: UInt64, points: [StrokePoint]) {
        Task { @MainActor [weak model] in
            model?.wetPoints(stroke: stroke, points: points)
        }
    }

    func wetEnd(stroke: String) {
        Task { @MainActor [weak model] in model?.wetEnd(stroke: stroke) }
    }

    func wetCancel(stroke: String) {
        Task { @MainActor [weak model] in model?.wetCancel(stroke: stroke) }
    }
}
