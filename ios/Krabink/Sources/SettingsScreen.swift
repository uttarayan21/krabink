// Settings sheet: sync status (with the way out of a workspace), this
// device (renamable), the synced device registry (removable rows), and
// pairing — show this device's QR for another device to scan, or join a
// workspace by pasting a `krabink://pair` URI.

import KrabinkCore
import SwiftUI

struct SettingsScreen: View {
    @Bindable var model: AppModel
    @State private var theme = ThemeStore.shared
    @Environment(\.dismiss) private var dismiss
    @State private var deviceToRemove: DeviceInfo?
    @State private var confirmUnpair = false
    @State private var nameEdit = ""
    @FocusState private var nameFocused: Bool
    @State private var joinURI = ""
    @State private var joinFailed = false
    @State private var showScanner = false

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    HStack(spacing: 14) {
                        ForEach(ThemeFlavor.allCases) { flavor in
                            Button {
                                theme.flavor = flavor
                            } label: {
                                ThemeSwatch(flavor: flavor, selected: flavor == theme.flavor)
                            }
                            .buttonStyle(.plain)
                            .accessibilityIdentifier("theme-\(flavor.rawValue)")
                        }
                    }
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 6)
                } header: {
                    Caption("Appearance")
                } footer: {
                    Text("Catppuccin flavours, lightest to darkest. Sketch paper follows the card colour on every device.")
                        .foregroundStyle(Theme.muted)
                }

                Section {
                    LabeledContent("state") {
                        HStack(spacing: 6) {
                            StatusDot(color: SyncTone(model.syncState).color)
                            Text(model.syncState)
                                .lineLimit(1)
                                .truncationMode(.middle)
                        }
                    }
                    if let paired = model.paired {
                        row("desktop", shortId(paired.node), mono: true)
                        row("relay", paired.relay ?? "none")
                        if let replica = paired.replica {
                            row("replica", shortId(replica), mono: true)
                        }
                    } else {
                        row("workspace", "not paired")
                    }
                    row("found nearby", model.discoveredAddr ?? "no")
                    ForEach(Array(model.peers.enumerated()), id: \.offset) { _, peer in
                        row(peerLabel(peer), peerState(peer))
                    }
                    if model.paired != nil {
                        Button(role: .destructive) {
                            confirmUnpair = true
                        } label: {
                            Label("unpair this iPad", systemImage: "xmark.circle")
                        }
                        .accessibilityIdentifier("unpair")
                    }
                } header: {
                    Caption("Sync")
                } footer: {
                    if model.paired != nil {
                        Text("unpairing removes this iPad from every device's list and stops syncing; notes already here stay")
                            .foregroundStyle(Theme.muted)
                    }
                }

                Section {
                    HStack {
                        Text("name")
                        TextField("device name", text: $nameEdit)
                            .multilineTextAlignment(.trailing)
                            .foregroundStyle(Theme.text)
                            .autocorrectionDisabled()
                            .submitLabel(.done)
                            .focused($nameFocused)
                            .onSubmit(saveName)
                            .accessibilityIdentifier("deviceName")
                        if nameEdit.trimmingCharacters(in: .whitespaces) != model.deviceName
                            && !nameEdit.trimmingCharacters(in: .whitespaces).isEmpty
                        {
                            Button("save", action: saveName)
                                .buttonStyle(.primary)
                                .accessibilityIdentifier("saveDeviceName")
                        }
                    }
                    row("id", shortId(model.core.deviceId()), mono: true)
                } header: {
                    Caption("This device")
                } footer: {
                    Text("every paired device shows this name in its list")
                        .foregroundStyle(Theme.muted)
                }

                Section {
                    if model.devices.isEmpty {
                        Text("no devices in this workspace yet")
                            .foregroundStyle(Theme.muted)
                    }
                    ForEach(model.devices, id: \.id) { device in
                        HStack(spacing: 12) {
                            Image(systemName: icon(for: device.platform))
                                .foregroundStyle(Theme.accent)
                                .frame(width: 22)
                            VStack(alignment: .leading, spacing: 2) {
                                Text(device.name)
                                    .foregroundStyle(Theme.text)
                                Text("\(device.platform) · \(seen(device.lastSeenMs))")
                                    .font(.caption)
                                    .foregroundStyle(Theme.muted)
                            }
                            Spacer()
                            if device.id == model.core.deviceId() {
                                Text("this device")
                                    .font(.caption)
                                    .foregroundStyle(Theme.muted)
                                    .padding(.horizontal, 8)
                                    .padding(.vertical, 3)
                                    .background(Capsule().fill(Theme.surfaceRaised))
                            } else {
                                Button {
                                    deviceToRemove = device
                                } label: {
                                    Image(systemName: "trash")
                                        .foregroundStyle(Theme.danger)
                                }
                                .buttonStyle(.borderless)
                                .accessibilityLabel("remove \(device.name)")
                                .accessibilityIdentifier("removeDevice")
                            }
                        }
                        .swipeActions(edge: .trailing) {
                            if device.id != model.core.deviceId() {
                                Button("remove", role: .destructive) {
                                    deviceToRemove = device
                                }
                            }
                        }
                    }
                } header: {
                    Caption("Paired devices")
                } footer: {
                    if model.devices.count > 1 {
                        Text("removing a device forgets it everywhere; it re-appears if it reconnects with the same token")
                            .foregroundStyle(Theme.muted)
                    }
                }

                Section {
                    PairScreen(uri: model.pairURI)
                        .frame(maxWidth: .infinity)
                } header: {
                    Caption("Pair a new device")
                }

                Section {
                    Button {
                        showScanner = true
                    } label: {
                        Label("scan pairing code", systemImage: "qrcode.viewfinder")
                    }
                    .accessibilityIdentifier("scanPairing")
                    TextField("krabink://pair?…", text: $joinURI)
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                        .font(.body.monospaced())
                        .accessibilityIdentifier("joinURI")
                    Button {
                        if model.adoptPair(uri: joinURI) {
                            joinFailed = false
                            joinURI = ""
                            refresh()
                        } else {
                            joinFailed = true
                        }
                    } label: {
                        Text("join")
                            .frame(maxWidth: .infinity)
                    }
                    .buttonStyle(.primary)
                    .disabled(joinURI.isEmpty)
                    .accessibilityIdentifier("joinWorkspace")
                    if joinFailed {
                        Text("not a valid pairing URI")
                            .font(.caption)
                            .foregroundStyle(Theme.danger)
                    }
                } header: {
                    Caption("Join another workspace")
                }
            }
            .scrollContentBackground(.hidden)
            .background(Theme.bg)
            .navigationTitle("settings")
            .toolbarBackground(Theme.bg, for: .navigationBar)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("done") { dismiss() }
                        .accessibilityIdentifier("settingsDone")
                }
            }
            .onAppear {
                nameEdit = model.deviceName
                refresh()
            }
            .confirmationDialog(
                "Unpair this iPad?",
                isPresented: $confirmUnpair,
                titleVisibility: .visible
            ) {
                Button("Unpair", role: .destructive) {
                    model.unpair()
                    refresh()
                }
                Button("Cancel", role: .cancel) {}
            } message: {
                Text("Removes this iPad from every device's list and stops syncing. Notes already here stay. Scan a pairing code to join again.")
            }
            .confirmationDialog(
                "Remove \(deviceToRemove?.name ?? "device")?",
                isPresented: Binding(
                    get: { deviceToRemove != nil },
                    set: { if !$0 { deviceToRemove = nil } }),
                titleVisibility: .visible
            ) {
                Button("Remove", role: .destructive) {
                    if let device = deviceToRemove {
                        model.removeDevice(id: device.id)
                        refresh()
                    }
                    deviceToRemove = nil
                }
                Button("Cancel", role: .cancel) { deviceToRemove = nil }
            } message: {
                Text("Forgets it on every device. It comes back if it reconnects with the same token.")
            }
            .sheet(isPresented: $showScanner) {
                ScanScreen { uri in
                    let adopted = model.adoptPair(uri: uri)
                    if adopted { refresh() }
                    return adopted
                }
            }
        }
        .preferredColorScheme(theme.flavor.colorScheme)
        .tint(Theme.accent)
    }

    private func row(_ label: String, _ value: String, mono: Bool = false) -> some View {
        LabeledContent(label) {
            Text(value)
                .font(mono ? .body.monospaced() : .body)
                .foregroundStyle(Theme.muted)
                .multilineTextAlignment(.trailing)
                .textSelection(.enabled)
        }
    }

    private func icon(for platform: String) -> String {
        switch platform.lowercased() {
        case "ios", "ipados", "ipad": "ipad"
        case "macos", "linux", "windows", "desktop": "desktopcomputer"
        default: "circle.hexagongrid"
        }
    }

    private func refresh() {
        model.refreshPeers()
    }

    private func saveName() {
        model.rename(nameEdit)
        nameEdit = model.deviceName
        nameFocused = false
    }

    private func peerLabel(_ peer: PeerInfo) -> String {
        let kind: String
        switch peer.kind {
        case .replica: kind = "replica"
        case .desktop: kind = "desktop"
        case .tablet: kind = "tablet"
        case .unknown: kind = "peer"
        }
        return peer.inbound ? "\(kind) (dialled us)" : kind
    }

    private func peerState(_ peer: PeerInfo) -> String {
        if let error = peer.error { return "rejected: \(error)" }
        guard peer.connected else { return "connecting" }
        switch peer.route {
        case .direct(let addr): return "direct \(addr)"
        case .relay: return "via relay"
        case nil: return "connected"
        }
    }

    private func shortId(_ id: String) -> String {
        String(id.prefix(8))
    }

    private func seen(_ ms: UInt64) -> String {
        guard ms > 0 else { return "never" }
        let date = Date(timeIntervalSince1970: TimeInterval(ms) / 1000)
        return date.formatted(.relative(presentation: .named))
    }
}

/// A flavour's page, card and accent as a small tile; the selected one
/// gets the current accent as its ring.
private struct ThemeSwatch: View {
    let flavor: ThemeFlavor
    let selected: Bool

    var body: some View {
        let p = flavor.palette
        VStack(spacing: 6) {
            ZStack(alignment: .bottomTrailing) {
                RoundedRectangle(cornerRadius: 8, style: .continuous)
                    .fill(p.bg)
                RoundedRectangle(cornerRadius: 5, style: .continuous)
                    .fill(p.surface)
                    .padding(6)
                Circle()
                    .fill(p.accent)
                    .frame(width: 12, height: 12)
                    .padding(10)
            }
            .frame(width: 64, height: 44)
            .overlay {
                RoundedRectangle(cornerRadius: 8, style: .continuous)
                    .stroke(selected ? Theme.accent : p.border, lineWidth: selected ? 2 : 1)
            }
            Text(flavor.label)
                .font(.caption2)
                .foregroundStyle(selected ? Theme.text : Theme.muted)
        }
        .contentShape(Rectangle())
        .accessibilityLabel(flavor.label)
        .accessibilityAddTraits(selected ? .isSelected : [])
    }
}
