// swift-tools-version:5.9
// Local SPM package wrapping the Rust core. `PendantCoreFFI.xcframework` and
// `Sources/PendantCore/Pendant.swift` are generated — run
// `scripts/build-ios-core.sh` (macOS) before building the app.
import PackageDescription

let package = Package(
    name: "PendantCore",
    platforms: [.iOS(.v17)],
    products: [
        .library(name: "PendantCore", targets: ["PendantCore"])
    ],
    targets: [
        .target(name: "PendantCore", dependencies: ["PendantCoreFFI"]),
        .binaryTarget(name: "PendantCoreFFI", path: "PendantCoreFFI.xcframework"),
    ]
)
