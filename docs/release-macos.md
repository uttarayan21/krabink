# Shipping the desktop app to the Mac App Store

The Mac build is the bevy/egui desktop client (`crates/krabink`), not a
Mac Catalyst port of the iPad app. It lives on the same App Store Connect
record as the iPad app (bundle id `dev.darksailor.krabink`, universal
purchase), so the one-time setup in `docs/release-ios.md` (paid program,
app record, API key, privacy policy, export compliance) already covers it.

## How the build works

Xcode compiles nothing. `macos/project.yml` is an XcodeGen spec for an
app target whose only build step copies the Rust binary into the bundle;
Xcode then does what the store needs: Info.plist, icon, sandbox
entitlements, cloud-managed signing, archive, `.pkg` export and upload.
That keeps the Mac pipeline identical to the iPad one (same team, same
automatic signing, no hand-made certificates or profiles).

| Piece | Where | Notes |
|---|---|---|
| Universal binary | `scripts/archive-macos.sh` | `cargo build --release` for `aarch64-apple-darwin` and `x86_64-apple-darwin`, `lipo`'d into `target/universal-apple-darwin/release/krabink`. The store rejects arm64-only native Mac apps. Linked with Apple's clang, `MACOSX_DEPLOYMENT_TARGET=12.0`. |
| App target | `macos/project.yml` | Info.plist (category, min macOS 12, local-network string + `_krabink._udp` Bonjour type), hardened runtime. `Krabink.xcodeproj`, `Info.plist` and `Krabink.entitlements` are generated and ignored. |
| Sandbox | `macos/project.yml` → `entitlements` | `app-sandbox`, `network.client`, `network.server`. Nothing else: no file dialogs, no camera, no subprocesses. |
| Icon | `macos/Assets.xcassets` | Every mac size scaled from the iPad icon by `scripts/gen-mac-icon.sh`. |
| Privacy manifest | `ios/Krabink/PrivacyInfo.xcprivacy` | Shared with the iPad app. |
| Launch args | `crates/krabink/src/main.rs` | `-psn_…` from LaunchServices is dropped before clap parses. |

## Sandbox behaviour

- **Data moves.** `ProjectDirs` resolves through `$HOME`, which the
  sandbox points at `~/Library/Containers/dev.darksailor.krabink/Data`.
  A store install starts with an empty workspace and a new node key; it
  does not see the stores of a `cargo run` build
  (`~/Library/Application Support/dev.darksailor.krabink`). Pair it like
  a new device.
- **No CLI flags.** Relay and token come from the pairing window's join
  field (a `krabink://pair?…` URI) or `config.toml` inside the container.
- **Local network prompt.** On macOS 15+ the first mDNS lookup shows the
  system dialog with `NSLocalNetworkUsageDescription`.

## Per release

1. Bump `MARKETING_VERSION` in `macos/project.yml` (with `Cargo.toml` and
   the iPad spec), commit.
2. `scripts/archive-macos.sh` exports
   `macos/build-archive/export/Krabink.pkg`;
   `KRABINK_EXPORT_DESTINATION=upload scripts/archive-macos.sh` uploads.
   Cold, both archs, expect 30+ minutes; warm, a few.
3. The build lands under TestFlight → macOS. Answer export compliance the
   same way as for iPad.

## Before the first review

- Try the sandboxed build before uploading: open the archive's app from
  `macos/build-archive/Krabink.xcarchive/Products/Applications/` and
  check that it launches, creates notes, shows the pairing QR and syncs
  with the iPad. A sandbox violation shows up in Console.app as
  `Sandbox: krabink deny(…)`.
- **Screenshots**: Mac sets at 2880×1800 (or 1280×800, 1440×900,
  2560×1600). Note list with a note open, a note with page ink, the
  pairing window.
- **Review notes**: as for iPad; add that the Mac app shows its pairing
  QR in the pairing window and the iPad scans it.
- **TestFlight on Mac** needs macOS 12+ and the TestFlight Mac app;
  public links work the same as on iPad once external review passes.
