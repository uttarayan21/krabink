// QR scanner for pairing: VisionKit's DataScannerViewController locked to QR
// symbology. The first recognized `krabink://pair` URI is handed to the
// caller and the sheet closes; non-krabink codes are ignored in place.
// Simulators (and denied camera permission) get an explanatory fallback —
// UI tests exercise pairing through the `-pairURI` launch argument instead.

import SwiftUI
import VisionKit

struct ScanScreen: View {
    @Environment(\.dismiss) private var dismiss
    /// Called with the raw scanned URI; returns whether it was adopted.
    let onScan: (String) -> Bool
    @State private var rejected = false

    var body: some View {
        NavigationStack {
            Group {
                if DataScannerViewController.isSupported,
                    DataScannerViewController.isAvailable
                {
                    QRScanner { uri in
                        if onScan(uri) {
                            dismiss()
                        } else {
                            rejected = true
                        }
                    }
                } else {
                    ContentUnavailableView(
                        "camera unavailable",
                        systemImage: "camera.fill",
                        description: Text(
                            "scanning needs a device camera and permission; "
                                + "you can paste the pairing URI in settings instead"))
                }
            }
            .navigationTitle("scan pairing code")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("cancel") { dismiss() }
                        .accessibilityIdentifier("scanCancel")
                }
            }
            .safeAreaInset(edge: .bottom) {
                if rejected {
                    Text("that code is not a krabink pairing QR")
                        .font(.footnote)
                        .foregroundStyle(.red)
                        .padding(.bottom, 8)
                }
            }
        }
    }
}

private struct QRScanner: UIViewControllerRepresentable {
    let onScan: (String) -> Void

    func makeCoordinator() -> Coordinator { Coordinator(onScan: onScan) }

    func makeUIViewController(context: Context) -> DataScannerViewController {
        let scanner = DataScannerViewController(
            recognizedDataTypes: [.barcode(symbologies: [.qr])],
            qualityLevel: .fast,
            isHighlightingEnabled: true)
        scanner.delegate = context.coordinator
        try? scanner.startScanning()
        return scanner
    }

    func updateUIViewController(_ scanner: DataScannerViewController, context: Context) {}

    @MainActor
    final class Coordinator: NSObject, DataScannerViewControllerDelegate {
        let onScan: (String) -> Void

        init(onScan: @escaping (String) -> Void) { self.onScan = onScan }

        func dataScanner(
            _ scanner: DataScannerViewController,
            didAdd added: [RecognizedItem],
            allItems: [RecognizedItem]
        ) {
            for item in added {
                if case .barcode(let barcode) = item,
                    let payload = barcode.payloadStringValue
                {
                    onScan(payload)
                }
            }
        }
    }
}
