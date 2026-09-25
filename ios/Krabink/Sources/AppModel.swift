// App-level state: owns the UniFFI Core, the note registry, and one
// NoteModel per open note. All FFI listener callbacks arrive on Rust's
// network thread and hop to the main actor before touching state.

import Foundation
import Network
import KrabinkCore
import UIKit

@Observable @MainActor
final class AppModel {
    let core: Core
    var notes: [NoteInfo] = []
    var syncState = "not paired"
    /// Every peer the node knows, refreshed on each sync-state change.
    var peers: [PeerInfo] = []
    /// The synced device registry (every device in the workspace, most
    /// recently seen first); pushed by the core whenever a peer changes it.
    var devices: [DeviceInfo] = []
    /// How this iPad shows up in every device's list. iOS hides the real
    /// device name from apps, so the default is generic until renamed.
    private(set) var deviceName: String
    /// The workspace adopted from a pairing URI; nil until paired.
    private(set) var paired: PairInfo?
    private var discovery: PeerDiscovery?
    private var open: [String: NoteModel] = [:]
    private let pathMonitor = NWPathMonitor()
    /// Suppress the very first path callback (fires with the initial state
    /// right after start).
    private var sawInitialPath = false

    init() {
        let dir = FileManager.default
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("krabink").path
        core = try! Core(dataDir: dir)
        deviceName = UserDefaults.standard.string(forKey: "deviceName") ?? UIDevice.current.name
        notes = core.listNotes()
        devices = core.listDevices()
        core.setListener(listener: CoreEvents(model: self))
        BrushLibrary.shared.attach(core)
        InkAssets.shared.setShared(core.listAssets())

        // The pairing persists as its URI; `-pairURI krabink://pair?…` as a
        // launch argument takes the same path as a scanned QR (UI tests
        // exercise pairing without a camera).
        if let uri = UserDefaults.standard.string(forKey: "pairURI") {
            _ = adoptPair(uri: uri)
        }

        // Tell the node when the network changes (Wi-Fi handoff, VPN,
        // airplane-mode off): it re-probes paths instead of waiting for
        // timeouts.
        pathMonitor.pathUpdateHandler = { [weak self] path in
            guard path.status == .satisfied else { return }
            Task { @MainActor [weak self] in
                guard let self else { return }
                if self.sawInitialPath { self.core.networkChanged() } else { self.sawInitialPath = true }
            }
        }
        pathMonitor.start(queue: .global(qos: .utility))
    }

    /// Adopt sync coordinates from a `krabink://pair` URI (QR scan, deep
    /// link, or `-pairURI` launch argument). Persists them so the next
    /// launch reconnects without re-pairing.
    func adoptPair(uri: String) -> Bool {
        guard let info = parsePairUri(uri: uri) else { return false }
        do {
            try core.setPairing(info: info)
        } catch {
            syncState = "error: \(error)"
            return false
        }
        UserDefaults.standard.set(uri, forKey: "pairURI")
        paired = info
        // The node may already hold a connection (a peer dialled us, or
        // the same workspace was re-adopted): read the state, don't guess.
        refreshSyncState()
        applyDiscovery(info)
        // Announce this device in the synced registry so peers can list it.
        registerDevice()
        return true
    }

    /// Upsert our row (name, platform, last seen) in the synced registry.
    /// Called on pair, on every (re)connect and after a rename, so a row a
    /// peer removed comes back and a new name reaches everyone.
    func registerDevice() {
        try? core.registerDevice(name: deviceName, platform: UIDevice.current.model)
        devices = core.listDevices()
    }

    /// Rename this iPad everywhere. Blank names are ignored.
    func rename(_ name: String) {
        let name = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !name.isEmpty, name != deviceName else { return }
        deviceName = name
        UserDefaults.standard.set(name, forKey: "deviceName")
        registerDevice()
    }

    /// Leave the workspace: our row disappears from every device's list,
    /// the pairing is forgotten (also for the next launch) and the node
    /// stops dialling. Notes already synced stay on this iPad.
    func unpair() {
        try? core.unpair()
        forgetPairing()
        syncState = "not paired"
    }

    /// The paired desktop removed this iPad: the core has already dropped
    /// the pairing (its QR token is back to our own), so forget it here too
    /// or the next launch would re-adopt the persisted URI.
    func reconcilePairing() {
        guard let paired, core.pairInfo().token != paired.token else { return }
        forgetPairing()
        syncState = "removed by the desktop"
    }

    private func forgetPairing() {
        UserDefaults.standard.removeObject(forKey: "pairURI")
        discovery?.stop()
        discovery = nil
        paired = nil
        devices = core.listDevices()
        refreshPeers()
    }

    /// What this device shows as a QR: its own node plus the adopted
    /// workspace's token and relay. Valid before pairing too (then it
    /// carries this device's own token and no relay).
    var pairInfo: PairInfo { core.pairInfo() }

    var pairURI: String { buildPairUri(info: pairInfo) }

    /// Direct address Bonjour found for the paired desktop, if any.
    var discoveredAddr: String? { discovery?.addr }

    /// Look for the paired desktop on the local network; a hit becomes an
    /// address hint for the node (only matters when the relay is down).
    private func applyDiscovery(_ info: PairInfo) {
        discovery?.stop()
        let finder = PeerDiscovery(nodeId: info.node)
        finder.onFound = { [weak self] addr in
            try? self?.core.addPeerAddr(node: info.node, addr: addr)
        }
        finder.start()
        discovery = finder
    }

    func refreshPeers() {
        peers = core.peers()
    }

    /// Registry row is gone everywhere once this syncs; note history stays
    /// in local stores (GC is backlog).
    func deleteNote(id: String) {
        try? core.deleteNote(id: id)
        open[id] = nil
        notes = core.listNotes()
    }

    /// Forget a paired device everywhere. It comes back if it reconnects
    /// with the same token; this is housekeeping, not revocation.
    func removeDevice(id: String) {
        try? core.removeDevice(id: id)
        devices = core.listDevices()
    }

    /// Foreground: bring the node back online and redial every peer.
    func resume() {
        discovery?.start()
        try? core.connect()
        refreshSyncState()
    }

    /// Relabel from the core's current aggregate state. Pushed changes
    /// keep it fresh afterwards; this covers the moments the label was
    /// set locally.
    func refreshSyncState() {
        syncState = CoreEvents.label(core.syncState(), paired: paired != nil)
    }

    /// Background: close every connection so iOS doesn't kill us holding
    /// them.
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

/// One open note: `text` mirrors the CRDT and drives the editor; `ink`
/// is the page ink drawn over it.
@Observable @MainActor
final class NoteModel: Identifiable {
    let session: NoteSession
    let id: String
    /// CRDT-authoritative text. Local typing updates it after the fact;
    /// remote changes update it first and the editor diffs against it.
    var text: String
    var synced = false
    /// The page ink layer: one model per note, kept with the note so ink
    /// state (tool, counters) survives switching notes.
    let ink: PageInkModel
    var onTitleChanged: (() -> Void)?
    private var titleTask: Task<Void, Never>?

    init(session: NoteSession) {
        self.session = session
        id = session.id()
        text = (try? session.text()) ?? ""
        ink = PageInkModel(session: session)
        session.setListener(listener: NoteEvents(model: self))
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

    func assetsChanged(assets: [AssetInfo]) {
        Task { @MainActor in InkAssets.shared.setShared(assets) }
    }

    func devicesChanged(devices: [DeviceInfo]) {
        Task { @MainActor [weak model] in
            guard let model else { return }
            model.devices = devices
            model.reconcilePairing()
        }
    }

    func syncState(state: SyncState) {
        Task { @MainActor [weak model] in
            guard let model else { return }
            model.reconcilePairing()
            model.syncState = Self.label(state, paired: model.paired != nil)
            model.refreshPeers()
            // Fresh connection: refresh our row (last seen, current name;
            // back if a peer removed it).
            if case .connected = state, model.paired != nil {
                model.registerDevice()
            }
        }
    }

    static func label(_ state: SyncState, paired: Bool) -> String {
        switch state {
        case .disconnected: return paired ? "offline" : "not paired"
        case .connecting: return "connecting"
        case .connected(_, let route):
            switch route {
            case .direct(let addr): return "connected direct \(addr)"
            case .relay: return "connected via relay"
            case nil: return "connected"
            }
        case .fatal(let message): return "error: \(message)"
        }
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

    func pageChanged() {
        Task { @MainActor [weak model] in model?.ink.remoteChanged() }
    }

    func strokesChanged(sketch: String) {
        // Inline sketches: wired to the ink model in the inline-sketch step.
        _ = sketch
    }

    func wetBegin(
        sketch: String, stroke: String, tool: Tool, color: UInt32, baseWidth: Float, spec: Data?
    ) {
        // Inline sketches: wired to the ink model in the inline-sketch step.
        _ = (sketch, stroke, tool, color, baseWidth, spec)
    }

    func wetBeginAnchored(
        stroke: String, anchor: Data, tool: Tool, color: UInt32, baseWidth: Float, spec: Data?
    ) {
        Task { @MainActor [weak model] in
            model?.ink.remoteWetBegin(
                stroke: stroke, anchor: anchor, tool: tool, color: color, baseWidth: baseWidth,
                spec: spec)
        }
    }

    func wetPoints(stroke: String, sentMs: UInt64, points: [StrokePoint]) {
        Task { @MainActor [weak model] in
            model?.ink.remoteWetPoints(stroke: stroke, points: points)
        }
    }

    func wetEnd(stroke: String) {
        Task { @MainActor [weak model] in model?.ink.remoteWetEnd(stroke: stroke) }
    }

    func wetCancel(stroke: String) {
        Task { @MainActor [weak model] in model?.ink.remoteWetCancel(stroke: stroke) }
    }
}
