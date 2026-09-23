// swift-tools-version:5.9
// Local SPM package wrapping the Rust core. `KrabinkCoreFFI.xcframework` and
// `Sources/KrabinkCore/Krabink.swift` are generated — run
// `scripts/build-ios-core.sh` (macOS) before building the app.
import PackageDescription

let package = Package(
    name: "KrabinkCore",
    platforms: [.iOS(.v17)],
    products: [
        .library(name: "KrabinkCore", targets: ["KrabinkCore"])
    ],
    targets: [
        // iroh (netdev, n0-dns-resolver) reads interfaces and DNS through
        // SystemConfiguration; a static lib cannot pull the framework in itself.
        .target(
            name: "KrabinkCore",
            dependencies: ["KrabinkCoreFFI"],
            linkerSettings: [.linkedFramework("SystemConfiguration")]
        ),
        .binaryTarget(name: "KrabinkCoreFFI", path: "KrabinkCoreFFI.xcframework"),
    ]
)
