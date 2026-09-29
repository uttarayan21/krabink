// Shared look for the iPad app, mirroring the desktop theme
// (crates/krabink/src/theme.rs): the four Catppuccin flavours with
// lavender as the accent, cards on a page, roomier padding. The chosen
// flavour lives in `ThemeStore.shared` (persisted in UserDefaults); every
// `Theme.*` colour reads it, so views observing the store restyle at once.
// The sketch paper (`UIColor.paper`) is the flavour's card colour unless
// the user picked another (`ThemeStore.paper`), the same value the desktop
// clears its render targets to; text on it takes the paper's tone.

import SwiftUI
import UIKit

/// One of the Catppuccin flavours: https://catppuccin.com/palette
enum ThemeFlavor: String, CaseIterable, Identifiable {
    case latte, frappe, macchiato, mocha

    var id: String { rawValue }

    var label: String {
        switch self {
        case .latte: "Latte"
        case .frappe: "Frappé"
        case .macchiato: "Macchiato"
        case .mocha: "Mocha"
        }
    }

    /// Latte is the light flavour; the rest are dark.
    var colorScheme: ColorScheme {
        self == .latte ? .light : .dark
    }

    var palette: Palette {
        switch self {
        case .latte: Palette.latte
        case .frappe: Palette.frappe
        case .macchiato: Palette.macchiato
        case .mocha: Palette.mocha
        }
    }
}

/// Every colour the app uses, by role. Catppuccin roles in brackets.
struct Palette {
    /// Window background [base].
    let bg: Color
    /// Note list and toolbars [mantle].
    let sidebar: Color
    /// Cards, editor, inputs and, by default, sketch paper [surface0].
    let surface: Color
    /// Hovered or selected rows, code [surface1].
    let surfaceRaised: Color
    /// Hairlines around cards [surface1].
    let border: Color
    /// Primary text [text].
    let text: Color
    /// Secondary text, captions, hints [subtext0].
    let muted: Color
    /// Brand accent: buttons, selection, links [lavender].
    let accent: Color
    /// Text on an accent-filled button [base].
    let onAccent: Color
    let success: Color
    let warn: Color
    let danger: Color

    /// Translucent accent behind the selected note.
    var accentSoft: Color { accent.opacity(0.28) }

    static let latte = Palette(
        bg: Color(hex: 0xEFF1F5), sidebar: Color(hex: 0xE6E9EF),
        surface: Color(hex: 0xCCD0DA), surfaceRaised: Color(hex: 0xBCC0CC),
        border: Color(hex: 0xBCC0CC), text: Color(hex: 0x4C4F69),
        muted: Color(hex: 0x6C6F85), accent: Color(hex: 0x7287FD),
        onAccent: Color(hex: 0xEFF1F5), success: Color(hex: 0x40A02B),
        warn: Color(hex: 0xDF8E1D), danger: Color(hex: 0xD20F39))

    static let frappe = Palette(
        bg: Color(hex: 0x303446), sidebar: Color(hex: 0x292C3C),
        surface: Color(hex: 0x414559), surfaceRaised: Color(hex: 0x51576D),
        border: Color(hex: 0x51576D), text: Color(hex: 0xC6D0F5),
        muted: Color(hex: 0xA5ADCE), accent: Color(hex: 0xBABBF1),
        onAccent: Color(hex: 0x303446), success: Color(hex: 0xA6D189),
        warn: Color(hex: 0xE5C890), danger: Color(hex: 0xE78284))

    static let macchiato = Palette(
        bg: Color(hex: 0x24273A), sidebar: Color(hex: 0x1E2030),
        surface: Color(hex: 0x363A4F), surfaceRaised: Color(hex: 0x494D64),
        border: Color(hex: 0x494D64), text: Color(hex: 0xCAD3F5),
        muted: Color(hex: 0xA5ADCB), accent: Color(hex: 0xB7BDF8),
        onAccent: Color(hex: 0x24273A), success: Color(hex: 0xA6DA95),
        warn: Color(hex: 0xEED49F), danger: Color(hex: 0xED8796))

    static let mocha = Palette(
        bg: Color(hex: 0x1E1E2E), sidebar: Color(hex: 0x181825),
        surface: Color(hex: 0x313244), surfaceRaised: Color(hex: 0x45475A),
        border: Color(hex: 0x45475A), text: Color(hex: 0xCDD6F4),
        muted: Color(hex: 0xA6ADC8), accent: Color(hex: 0xB4BEFE),
        onAccent: Color(hex: 0x1E1E2E), success: Color(hex: 0xA6E3A1),
        warn: Color(hex: 0xF9E2AF), danger: Color(hex: 0xF38BA8))
}

/// A look as the workspace shares it (`AppearanceInfo` over the FFI):
/// flavour by raw value, paper as `0xRRGGBB`.
struct SharedLook: Equatable {
    var flavor: String
    var paper: UInt32?
}

/// The chosen flavour and paper, persisted as `theme` and `paper` in
/// UserDefaults. Views that read any `Theme.*` colour in their body
/// observe it and restyle when it changes; UIKit-backed views take the
/// flavour and paper as inputs so their `updateUIView` runs too.
///
/// With `syncTheme` on (the default) the look is the workspace's: local
/// changes are shared through the core, and a shared look that changes
/// is applied here. The desktop does the same (`ThemeSync` in
/// crates/krabink/src/theme.rs).
@Observable
final class ThemeStore {
    static let shared = ThemeStore()

    var flavor: ThemeFlavor {
        didSet {
            UserDefaults.standard.set(flavor.rawValue, forKey: "theme")
            localChanged()
        }
    }

    /// Picked paper as `0xRRGGBB`, kept across flavour switches; `nil`
    /// follows the flavour's card colour.
    var paper: UInt32? {
        didSet {
            if let paper {
                UserDefaults.standard.set(Int(paper), forKey: "paper")
            } else {
                UserDefaults.standard.removeObject(forKey: "paper")
            }
            localChanged()
        }
    }

    /// Follow and share the workspace's look. Turning it on takes the
    /// workspace's look if it has one, else offers this iPad's.
    var syncTheme: Bool {
        didSet {
            UserDefaults.standard.set(syncTheme, forKey: "syncTheme")
            lastShared = nil
            pendingShare?.cancel()
            guard syncTheme else { return }
            if let shared = readShared?() {
                sharedChanged(shared)
            } else {
                shareSoon(after: 0)
            }
        }
    }

    /// The workspace's look through the core; wired up by `AppModel`.
    @ObservationIgnored var readShared: (() -> SharedLook?)?
    @ObservationIgnored var writeShared: ((SharedLook) -> Void)?
    /// The shared look last seen. Only a change is applied, so a local
    /// pick not yet shared (the wheel mid-drag) is not snapped back by an
    /// unrelated workspace update.
    @ObservationIgnored private var lastShared: SharedLook?
    @ObservationIgnored private var applyingShared = false
    @ObservationIgnored private var pendingShare: DispatchWorkItem?

    /// This device's look, as the workspace would share it.
    var look: SharedLook { SharedLook(flavor: flavor.rawValue, paper: paper) }

    /// The workspace's look as of the latest workspace update: applied
    /// when it changed since last seen and this iPad syncs its theme.
    func sharedChanged(_ shared: SharedLook?) {
        guard syncTheme, shared != lastShared else { return }
        lastShared = shared
        guard let shared, shared != look,
            let flavor = ThemeFlavor(rawValue: shared.flavor)
        else { return }
        // The peer's pick is newer than whatever was waiting to go out.
        pendingShare?.cancel()
        applyingShared = true
        self.flavor = flavor
        paper = shared.paper
        applyingShared = false
    }

    /// Share a local pick, debounced so dragging the wheel sends only
    /// where it settles.
    private func localChanged() {
        guard syncTheme, !applyingShared else { return }
        shareSoon(after: 0.3)
    }

    private func shareSoon(after delay: TimeInterval) {
        pendingShare?.cancel()
        let item = DispatchWorkItem { [weak self] in
            guard let self, self.syncTheme else { return }
            self.writeShared?(self.look)
        }
        pendingShare = item
        DispatchQueue.main.asyncAfter(deadline: .now() + delay, execute: item)
    }

    var palette: Palette { flavor.palette }

    /// The page's colour: the picked paper, else the flavour's card.
    var paperColor: Color { paper.map { Color(hex: $0) } ?? palette.surface }

    /// `paperColor` as `0xRRGGBB`.
    var paperHex: UInt32 { paper ?? UIColor(palette.surface).rgbHex }

    /// Colours for what sits on the paper (editor text, inline frames):
    /// the flavour's while the paper keeps its tone, else Latte's (light
    /// paper) or Mocha's (dark paper). Mirrors `Palette::on_paper` in
    /// crates/krabink/src/theme.rs.
    var paperPalette: Palette {
        let flavorDark = flavor.colorScheme == .dark
        let paperDark = paper.map(Self.isDark) ?? flavorDark
        if paperDark == flavorDark { return palette }
        return paperDark ? .mocha : .latte
    }

    /// Linear luminance under 0.5: the cut-off `InkRenderer.isDark` and
    /// the desktop use.
    static func isDark(_ hex: UInt32) -> Bool {
        func linear(_ byte: UInt32) -> Double {
            let c = Double(byte & 0xFF) / 255
            return c <= 0.04045 ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4)
        }
        return 0.2126 * linear(hex >> 16) + 0.7152 * linear(hex >> 8) + 0.0722 * linear(hex) < 0.5
    }

    private init() {
        let defaults = UserDefaults.standard
        let stored = defaults.string(forKey: "theme") ?? ""
        flavor = ThemeFlavor(rawValue: stored) ?? .mocha
        paper = (defaults.object(forKey: "paper") as? Int).map { UInt32(truncatingIfNeeded: $0) & 0xFF_FFFF }
        syncTheme = defaults.object(forKey: "syncTheme") as? Bool ?? true
    }
}

enum Theme {
    private static var palette: Palette { ThemeStore.shared.palette }

    /// Window background.
    static var bg: Color { palette.bg }
    /// Note list and toolbars.
    static var sidebar: Color { palette.sidebar }
    /// Cards, inputs; the paper too unless another was picked.
    static var surface: Color { palette.surface }
    /// The page under notes: `UIColor.paper`.
    static var paper: Color { ThemeStore.shared.paperColor }
    /// Hovered or selected rows, code.
    static var surfaceRaised: Color { palette.surfaceRaised }
    /// Hairlines around cards.
    static var border: Color { palette.border }
    /// Primary text.
    static var text: Color { palette.text }
    /// Secondary text, captions, hints.
    static var muted: Color { palette.muted }
    /// Brand accent: buttons, selection, links.
    static var accent: Color { palette.accent }
    /// Text on an accent-filled button.
    static var onAccent: Color { palette.onAccent }
    /// Translucent accent behind the selected note.
    static var accentSoft: Color { palette.accentSoft }
    static var success: Color { palette.success }
    static var warn: Color { palette.warn }
    static var danger: Color { palette.danger }

    /// Corner radius shared by cards, tiles and buttons.
    static let radius: CGFloat = 10
    /// Padding around the editor and preview cards.
    static let pagePadding: CGFloat = 16
}

extension Color {
    init(hex: UInt32) {
        self.init(
            red: Double((hex >> 16) & 0xFF) / 255,
            green: Double((hex >> 8) & 0xFF) / 255,
            blue: Double(hex & 0xFF) / 255)
    }
}

/// UIKit twins for the UIView-backed note canvas.
extension UIColor {
    static var themeBg: UIColor { UIColor(Theme.bg) }
    static var themeSurface: UIColor { UIColor(Theme.surface) }
    static var themeText: UIColor { UIColor(Theme.text) }

    /// Page paper: the picked colour or the flavour's card colour, the
    /// desktop's `Palette::paper` in crates/krabink/src/theme.rs. Both
    /// platforms clear the ink layer to this, under the text, so ink
    /// reads alike everywhere; the renderer picks the highlighter blend
    /// from its luminance.
    static var paper: UIColor { UIColor(Theme.paper) }

    // What sits on the paper, in its tone (`ThemeStore.paperPalette`).
    private static var onPaper: Palette { ThemeStore.shared.paperPalette }
    static var paperText: UIColor { UIColor(onPaper.text) }
    static var paperMuted: UIColor { UIColor(onPaper.muted) }
    static var paperAccent: UIColor { UIColor(onPaper.accent) }
    static var paperBorder: UIColor { UIColor(onPaper.border) }
    /// Code spans and blocks in the editor.
    static var paperSurfaceRaised: UIColor { UIColor(onPaper.surfaceRaised) }

    /// The sRGB components as `0xRRGGBB`, the inverse of `Color(hex:)`.
    var rgbHex: UInt32 {
        var r: CGFloat = 0
        var g: CGFloat = 0
        var b: CGFloat = 0
        var a: CGFloat = 0
        getRed(&r, green: &g, blue: &b, alpha: &a)
        func byte(_ c: CGFloat) -> UInt32 { UInt32(min(max((c * 255).rounded(), 0), 255)) }
        return byte(r) << 16 | byte(g) << 8 | byte(b)
    }
}

/// How the app reads a sync status line (`AppModel.syncState`).
enum SyncTone {
    case online, connecting, error, offline

    init(_ state: String) {
        if state.hasPrefix("connected") {
            self = .online
        } else if state.hasPrefix("connecting") {
            self = .connecting
        } else if state.hasPrefix("error") {
            self = .error
        } else {
            self = .offline
        }
    }

    var color: Color {
        switch self {
        case .online: Theme.success
        case .connecting: Theme.warn
        case .error: Theme.danger
        case .offline: Theme.muted
        }
    }

    /// Short label for the editor header; the full status stays in the
    /// note list footer.
    var short: String {
        switch self {
        case .online: "live sync"
        case .connecting: "connecting…"
        case .error: "sync error"
        case .offline: "offline"
        }
    }
}

/// Small coloured circle next to a status label.
struct StatusDot: View {
    let color: Color

    var body: some View {
        Circle()
            .fill(color)
            .frame(width: 8, height: 8)
            .shadow(color: color.opacity(0.6), radius: 3)
    }
}

/// Uppercase section label, as on the desktop.
struct Caption: View {
    let text: String

    init(_ text: String) { self.text = text }

    var body: some View {
        Text(text.uppercased())
            .font(.caption.weight(.semibold))
            .tracking(1)
            .foregroundStyle(Theme.muted)
    }
}

/// App mark: rounded accent tile with a pen glyph.
struct LogoMark: View {
    var size: CGFloat = 28

    var body: some View {
        RoundedRectangle(cornerRadius: size * 0.3, style: .continuous)
            .fill(
                LinearGradient(
                    colors: [Theme.accent, Theme.accent.opacity(0.6)],
                    startPoint: .topLeading, endPoint: .bottomTrailing)
            )
            .frame(width: size, height: size)
            .overlay {
                Image(systemName: "pencil.tip")
                    .font(.system(size: size * 0.5, weight: .semibold))
                    .foregroundStyle(Theme.onAccent)
            }
            .shadow(color: Theme.accent.opacity(0.35), radius: 6, y: 2)
    }
}

/// Filled accent button with the palette's on-accent text (SwiftUI's
/// `.borderedProminent` insists on white, unreadable on pale lavender).
struct PrimaryButtonStyle: ButtonStyle {
    @Environment(\.isEnabled) private var enabled

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.subheadline.weight(.semibold))
            .foregroundStyle(Theme.onAccent)
            .padding(.horizontal, 16)
            .padding(.vertical, 9)
            .background(
                RoundedRectangle(cornerRadius: Theme.radius, style: .continuous)
                    .fill(Theme.accent)
            )
            .opacity(enabled ? (configuration.isPressed ? 0.75 : 1) : 0.45)
    }
}

extension ButtonStyle where Self == PrimaryButtonStyle {
    static var primary: PrimaryButtonStyle { PrimaryButtonStyle() }
}

/// Bordered, rounded surface that editor, preview and settings rows sit on.
struct CardBackground: ViewModifier {
    var fill: Color = Theme.surface

    func body(content: Content) -> some View {
        content
            .background(fill)
            .clipShape(RoundedRectangle(cornerRadius: Theme.radius, style: .continuous))
            .overlay {
                RoundedRectangle(cornerRadius: Theme.radius, style: .continuous)
                    .stroke(Theme.border, lineWidth: 1)
            }
    }
}

extension View {
    func card(fill: Color = Theme.surface) -> some View {
        modifier(CardBackground(fill: fill))
    }
}
