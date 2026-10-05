# Shipping the iPad and iPhone app to the App Store

What the repo already carries for a store build, what still lives outside
it, and the exact commands. Pair this with `docs/architecture.md` for what
the app actually does.

## In the repo

| Piece | Where | Notes |
|---|---|---|
| App icon | `ios/Krabink/Assets.xcassets/AppIcon.appiconset/AppIcon.png` | 1024×1024 opaque RGB, rendered by `cargo xtask gen icon ios` (pure Rust, `xtask/src/icon.rs`n, no deps). Regenerate after changing `LogoMark`/`Theme.accent`. |
| Privacy manifest | `ios/Krabink/PrivacyInfo.xcprivacy` | No tracking, no collected data; required-reason APIs: UserDefaults (CA92.1), file timestamps (C617.1), system boot time (35F9.1). |
| Version / build | `project.yml` → `MARKETING_VERSION`, `CURRENT_PROJECT_VERSION` | Marketing version is hand-bumped with `Cargo.toml`. Build number = `git rev-list --count HEAD`, set by `cargo xtask archive ios`. |
| Store metadata in Info.plist | `project.yml` → `info.properties` | Display name, productivity category, launch screen, orientations, usage strings (camera, local network, Bonjour), `krabink://` URL scheme. No encryption key until ASC issues a compliance code (see below). |
| Release config | `project.yml` → `settings.configs.Release` | dSYMs. The app is universal (`TARGETED_DEVICE_FAMILY: 1,2` in the base settings): one build for iPad and iPhone. |
| Dev screens | `KrabinkApp.swift` | Spike and brush-lab screens are `#if DEBUG`; store builds cannot reach them. |
| Archive + export | `cargo xtask archive ios` (`paseo run archive-ios`) | Rebuilds the Rust core, archives Release, exports an `.ipa` or uploads to App Store Connect. |

## Outside the repo (one-time, in this order)

1. **Paid Apple Developer Program** membership for team `YD2FVR5QH2`
   (the free team used for device deploys cannot upload to App Store
   Connect). Wait for the "Distribution" capability to appear.
2. **App record in App Store Connect**: bundle id `dev.darksailor.krabink`,
   name "Krabink", primary language, SKU. Register the bundle id in the
   developer portal first if ASC does not offer it.
3. **App Store Connect API key** (Users and Access → Integrations → App
   Store Connect API, role App Manager). Put the `.p8` on the Mac at
   `~/.private_keys/AuthKey_<KEYID>.p8`; the archive tasks take
   `KRABINK_ASC_KEY_PATH`, `KRABINK_ASC_KEY_ID`, `KRABINK_ASC_ISSUER_ID`
   (or `--asc-key-path` etc.).
   Without it, upload uses the Xcode account session on the Mac.
4. **Privacy policy URL** (mandatory for every app). One paragraph is
   enough: notes and sketches stay on your devices and your own relay; no
   analytics, no accounts, no third parties.
5. **Export compliance**: the core ships rustls/ring (iroh's QUIC TLS), so
   the honest answer to "uses non-exempt encryption" is yes. On the first
   upload ASC asks the questions; answer "standard algorithms, not
   proprietary", file the annual self-classification report with BIS
   (standard for open-source TLS). Until ASC issues an
   `ITSEncryptionExportComplianceCode`, `project.yml` leaves
   `ITSAppUsesNonExemptEncryption` out entirely: `true` without a code is
   rejected at upload (ITMS-90592). Once a code exists, set both keys so
   later builds skip the prompt.

## Per release

1. `cargo xtask bump patch` (or `minor`/`major`): bumps `Cargo.toml` and
   `MARKETING_VERSION` in both `project.yml` files. Commit, merge.
2. `cargo xtask release --tag` (`xtask/src/release.rs`): checks the
   tree is clean and the versions agree, tags `v<version>` at HEAD and
   pushes it (which also starts the Gitea Linux package build), then
   archives and uploads the iPad and Mac apps with one shared build
   number (the commit count). From Linux the builds run on the Mac
   (see `xtask/src/mac.rs`). `--ios`/`--mac` does one platform,
   `--export` only exports (`ios/Krabink/build-archive/export/Krabink.ipa`),
   `--clean` runs `cargo clean` afterwards. About 10 minutes per platform
   with a warm cargo cache, 25+ cold.
   The underlying tasks still work on their own:
   `cargo xtask archive ios --destination upload`.
3. In ASC: attach the build to the version, fill "What's New", submit.
   TestFlight is the same build; add internal testers on the build page.

## TestFlight

A build shows up under TestFlight 5–30 minutes after the upload, once
processing finishes.

- **Missing Compliance**: with no `ITSAppUsesNonExemptEncryption` key
  in Info.plist, every build stops at "Missing Compliance" until the
  encryption questions are answered on the build page. Testers cannot install it before that.
- **Internal testers** (App Store Connect users on the team, up to 100)
  get the build as soon as compliance is answered; no review.
- **External testers** (email invites or a public link, up to 10,000)
  need Beta App Review for the first build of each version. It uses the
  "Test Information" page:
  - Beta App Description: "Krabink is a markdown notebook for iPad and
    iPhone where ink (Apple Pencil, or a finger on iPhone) lives on the
    page next to the text. Notes and ink
    sync peer-to-peer between your own devices; no account."
  - Feedback email and a contact (name, phone, email) for the reviewer.
  - Sign-in: not required.
  - Review notes: the same text as the App Store review notes below.
- **What to Test** (per build, shown to testers): write the headline
  changes since the last uploaded build, plus the standing asks: write
  and sketch in a note, check the reading view, pair with a desktop and
  confirm text and ink sync both ways, report the local-network prompt
  if it never appears.
- Testers send feedback from the TestFlight app (screenshot + text) and
  crashes arrive symbolicated under TestFlight → Crashes, since the
  archive uploads dSYMs.

## App Store Connect form answers

- **App Privacy**: "Data Not Collected". The device registry (name,
  platform, last seen) syncs only between the user's own devices and
  their own relay; nothing reaches the developer.
- **Age rating**: none of the content flags apply → 4+.
- **Category**: Productivity.
- **Screenshots**: a universal app needs an iPad 13" set and an iPhone
  6.9" set (ASC scales both down for the smaller sizes). The iPhone set
  does not exist yet: capture it before the first universal submission
  (note list, a note in the editor, the same note in draw mode with the
  tool picker up, settings). iPad: capture from the archive build on the
  M4 iPad Pro: note
  list with a note open, the reading view of a note with ink on it, the
  page mid-stroke with the keyboard up, settings with a paired desktop.
  A simulator set with placeholder notes and ink lives in
  `docs/screenshots/ipad/` (git LFS; 2752×2064, iPad Pro 13" landscape).
  `UITests/ScreenshotUITests.swift` regenerates it: on a fresh iPad Pro
  13" simulator, `xcodebuild test -configuration Release
  -only-testing:KrabinkUITests/ScreenshotUITests -resultBundlePath …`,
  then `xcrun xcresulttool export attachments`. The exported PNGs carry
  an orientation tag rather than rotated pixels: strip the `eXIf`/`iTXt`
  chunks and `sips -r 270` them before uploading.
- **Review notes**: the app is fully usable unpaired (local notes and
  ink). Pairing needs a second device running the desktop app or
  `krabink-server`; say so, and that no account exists. If review asks
  for a demo of sync, point a `krabink-server --dev` at a public relay and
  put its `krabink://pair?…` URI in the notes: Settings → "join" accepts
  it without a camera.
- **Sign-in**: none. **Ads**: none. **IDFA**: no.

## Checks before uploading

- `cargo xtask check ios --configuration Release` compiles the store
  configuration for the simulator (dev screens compiled out).
- `cargo xtask run ios` still installs the Debug build on the iPad
  (or a connected iPhone, `--device <udid>` to pick one);
  the store build is the same code with `-Osize`/whole-module Swift and
  the release Rust core.
- Local network prompt: first sync on a new iPad shows the iOS "local
  network" dialog with the `NSLocalNetworkUsageDescription` text; review
  expects the string to explain the Bonjour lookup, which it does.
