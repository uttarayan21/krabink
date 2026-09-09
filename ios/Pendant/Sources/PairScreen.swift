// Pairing sheet: renders this device's `pendant://pair` URI as a QR code.
// The other device scans it with its camera (the URL scheme opens Pendant)
// or, for desktop, runs `pendant pair '<uri>'`.

import CoreImage.CIFilterBuiltins
import SwiftUI

struct PairScreen: View {
    let uri: String

    var body: some View {
        VStack(spacing: 16) {
            Text("pair a device")
                .font(.headline)
            if let image = qrImage(for: uri) {
                Image(uiImage: image)
                    .interpolation(.none)
                    .resizable()
                    .scaledToFit()
                    .frame(maxWidth: 280)
                    .accessibilityIdentifier("pairQR")
            }
            Text("scan with the other device's camera,\nor run: pendant pair '<uri>'")
                .font(.footnote)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
            Text(uri)
                .font(.caption.monospaced())
                .textSelection(.enabled)
                .padding(.horizontal)
        }
        .padding()
    }

    private func qrImage(for uri: String) -> UIImage? {
        let filter = CIFilter.qrCodeGenerator()
        filter.message = Data(uri.utf8)
        guard let output = filter.outputImage else { return nil }
        // Scale up so the resizable Image isn't blurred from a tiny source.
        let scaled = output.transformed(by: CGAffineTransform(scaleX: 8, y: 8))
        guard let cg = CIContext().createCGImage(scaled, from: scaled.extent) else { return nil }
        return UIImage(cgImage: cg)
    }
}
