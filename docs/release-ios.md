# Shipping the iPad app to the App Store

What the repo already carries for a store build, what still lives outside
it, and the exact commands. Pair this with `docs/architecture.md` for what
the app actually does.

## In the repo

| Piece | Where | Notes |
|---|---|---|
| App icon | `ios/Krabink/Assets.xcassets/AppIcon.appiconset/AppIcon.png` | 1024×1024 opaque RGB, rendered by `scripts/gen-app-icon.py` (pure Python, no deps). Regenerate after changing `LogoMark`/`Theme.accent`. |
| Privacy manifest | `ios/Krabink/PrivacyInfo.xcprivacy` | No tracking, no collected data; required-reason APIs: UserDefaults (CA92.1), file timestamps (C617.1), system boot time (35F9.1). |
| Version / build | `project.yml` → `MARKETING_VERSION`, `CURRENT_PROJECT_VERSION` | Marketing version is hand-bumped with `Cargo.toml`. Build number = `git rev-list --count HEAD`, set by `scripts/archive-ios.sh`. |
| Store metadata in Info.plist | `project.yml` → `info.properties` | Display name, productivity category, launch screen, orientations, usage strings (camera, local network, Bonjour), `krabink://` URL scheme, `ITSAppUsesNonExemptEncryption`. |
| Release config | `project.yml` → `settings.configs.Release` | iPad-only (`TARGETED_DEVICE_FAMILY: 2`), dSYMs. Debug stays universal for iPhone simulator checks. |
| Dev screens | `KrabinkApp.swift` | Spike and brush-lab screens are `#if DEBUG`; store builds cannot reach them. |
| Archive + export | `scripts/archive-ios.sh` (`paseo run archive-ios`) | Rebuilds the Rust core, archives Release, exports an `.ipa` or uploads to App Store Connect. |

## Outside the repo (one-time, in this order)

1. **Paid Apple Developer Program** membership for team `YD2FVR5QH2`
   (the free team used for device deploys cannot upload to App Store
   Connect). Wait for the "Distribution" capability to appear.
2. **App record in App Store Connect**: bundle id `dev.darksailor.krabink`,
   name "Krabink", primary language, SKU. Register the bundle id in the
   developer portal first if ASC does not offer it.
3. **App Store Connect API key** (Users and Access → Integrations → App
   Store Connect API, role App Manager). Put the `.p8` on the Mac at
   `~/.private_keys/AuthKey_<KEYID>.p8`; the archive script takes
   `KRABINK_ASC_KEY_PATH`, `KRABINK_ASC_KEY_ID`, `KRABINK_ASC_ISSUER_ID`.
   Without it, upload uses the Xcode account session on the Mac.
4. **Privacy policy URL** (mandatory for every app). One paragraph is
   enough: notes and sketches stay on your devices and your own relay; no
   analytics, no accounts, no third parties.
5. **Export compliance**: the core ships rustls/ring (iroh's QUIC TLS), so
   the honest answer to "uses non-exempt encryption" is yes. On the first
   upload ASC asks the questions; answer "standard algorithms, not
   proprietary", file the annual self-classification report with BIS
   (standard for open-source TLS), and paste the returned
   `ITSEncryptionExportComplianceCode` into `project.yml` so later builds
   skip the prompt.

## Per release

1. Bump `MARKETING_VERSION` in `ios/Krabink/project.yml` (and
   `Cargo.toml`), commit.
2. `KRABINK_EXPORT_DESTINATION=upload scripts/archive-ios.sh`
   (from Linux; runs on the Mac through `scripts/on-mac.sh`). About 10
   minutes with a warm cargo cache, 25 cold. Without the env var it exports
   `ios/Krabink/build-archive/export/Krabink.ipa` instead.
3. In ASC: attach the build to the version, fill "What's New", submit.
   TestFlight is the same build; add internal testers on the build page.

## App Store Connect form answers

- **App Privacy**: "Data Not Collected". The device registry (name,
  platform, last seen) syncs only between the user's own devices and
  their own relay; nothing reaches the developer.
- **Age rating**: none of the content flags apply → 4+.
- **Category**: Productivity.
- **Screenshots**: iPad 13" and 12.9" (2nd/3rd gen) sets are required for
  iPad-only apps. Capture from the archive build on the M4 iPad Pro: note
  list with a note open, the reading view of a note with ink on it, the
  page mid-stroke with the keyboard up, settings with a paired desktop.
- **Review notes**: the app is fully usable unpaired (local notes and
  ink). Pairing needs a second device running the desktop app or
  `krabink-server`; say so, and that no account exists. If review asks
  for a demo of sync, point a `krabink-server --dev` at a public relay and
  put its `krabink://pair?…` URI in the notes: Settings → "join" accepts
  it without a camera.
- **Sign-in**: none. **Ads**: none. **IDFA**: no.

## Checks before uploading

- `KRABINK_CONFIGURATION=Release scripts/check-ipad.sh` compiles the store
  configuration for the simulator (iPad-only, dev screens compiled out).
- `scripts/deploy-ipad.sh` still installs the Debug build on the iPad;
  the store build is the same code with `-Osize`/whole-module Swift and
  the release Rust core.
- Local network prompt: first sync on a new iPad shows the iOS "local
  network" dialog with the `NSLocalNetworkUsageDescription` text; review
  expects the string to explain the Bonjour lookup, which it does.
