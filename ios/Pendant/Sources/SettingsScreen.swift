// Settings sheet: sync status, this device, the synced device registry, and
// pairing — show this device's QR for another device to scan, or join a
// workspace by pasting a `pendant://pair` URI.

import PendantCore
import SwiftUI

struct SettingsScreen: View {
    @Bindable var model: AppModel
    @Environment(\.dismiss) private var dismiss
    @State private var devices: [DeviceInfo] = []
    @State private var joinURI = ""
    @State private var joinFailed = false
    @State private var showScanner = false

    var body: some View {
        NavigationStack {
            Form {
                Section("sync") {
                    LabeledContent("state", value: model.syncState)
                    LabeledContent(
                        "server",
                        value: UserDefaults.standard.string(forKey: "serverURL") ?? "not set")
                    LabeledContent(
                        "fallback relay",
                        value: UserDefaults.standard.string(forKey: "fallbackURL") ?? "none")
                }

                Section("this device") {
                    LabeledContent("name", value: UIDevice.current.name)
                    LabeledContent("id", value: shortId(model.core.deviceId()))
                        .font(.body.monospaced())
                }

                Section("paired devices") {
                    if devices.isEmpty {
                        Text("no devices in this workspace yet")
                            .foregroundStyle(.secondary)
                    }
                    ForEach(devices, id: \.id) { device in
                        HStack {
                            VStack(alignment: .leading) {
                                Text(device.name)
                                Text("\(device.platform) · \(seen(device.lastSeenMs))")
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                            }
                            Spacer()
                            if device.id == model.core.deviceId() {
                                Text("this device")
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                            }
                        }
                    }
                }

                if let uri = model.pairURI {
                    Section("pair a new device") {
                        PairScreen(uri: uri)
                            .frame(maxWidth: .infinity)
                    }
                }

                Section("join another workspace") {
                    Button {
                        showScanner = true
                    } label: {
                        Label("scan pairing code", systemImage: "qrcode.viewfinder")
                    }
                    .accessibilityIdentifier("scanPairing")
                    TextField("pendant://pair?…", text: $joinURI)
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                        .font(.body.monospaced())
                        .accessibilityIdentifier("joinURI")
                    Button("join") {
                        if model.adoptPair(uri: joinURI) {
                            joinFailed = false
                            joinURI = ""
                            refresh()
                        } else {
                            joinFailed = true
                        }
                    }
                    .disabled(joinURI.isEmpty)
                    .accessibilityIdentifier("joinWorkspace")
                    if joinFailed {
                        Text("not a valid pairing URI")
                            .font(.caption)
                            .foregroundStyle(.red)
                    }
                }
            }
            .navigationTitle("settings")
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("done") { dismiss() }
                        .accessibilityIdentifier("settingsDone")
                }
            }
            .onAppear(perform: refresh)
            .sheet(isPresented: $showScanner) {
                ScanScreen { uri in
                    let adopted = model.adoptPair(uri: uri)
                    if adopted { refresh() }
                    return adopted
                }
            }
        }
    }

    private func refresh() {
        devices = model.devices()
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
