// Shared look for the iPad app, mirroring the desktop theme
// (crates/krabink/src/theme.rs): the four Catppuccin flavours with
// lavender as the accent, cards on a page, roomier padding. The chosen
// flavour lives in `ThemeStore.shared` (persisted in UserDefaults); every
// `Theme.*` colour reads it, so views observing the store restyle at once.
// The sketch paper (`UIColor.paper`) is the flavour's card colour, the
// same value the desktop clears its render targets to.

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
    /// Cards, editor, inputs and sketch paper [surface0].
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

/// The chosen flavour, persisted as `theme` in UserDefaults. Views that
/// read any `Theme.*` colour in their body observe it and restyle when it
/// changes; UIKit-backed views take the flavour as an input so their
/// `updateUIView` runs too.
@Observable
final class ThemeStore {
    static let shared = ThemeStore()

    var flavor: ThemeFlavor {
        didSet { UserDefaults.standard.set(flavor.rawValue, forKey: "theme") }
    }

    var palette: Palette { flavor.palette }

    private init() {
        let stored = UserDefaults.standard.string(forKey: "theme") ?? ""
        flavor = ThemeFlavor(rawValue: stored) ?? .mocha
    }
}

enum Theme {
    private static var palette: Palette { ThemeStore.shared.palette }

    /// Window background.
    static var bg: Color { palette.bg }
    /// Note list and toolbars.
    static var sidebar: Color { palette.sidebar }
    /// Cards, editor, inputs. Equal to `UIColor.paper`.
    static var surface: Color { palette.surface }
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
    /// Code spans and blocks in the editor.
    static var themeSurfaceRaised: UIColor { UIColor(Theme.surfaceRaised) }
    static var themeText: UIColor { UIColor(Theme.text) }
    static var themeMuted: UIColor { UIColor(Theme.muted) }
    static var themeBorder: UIColor { UIColor(Theme.border) }
    static var themeAccent: UIColor { UIColor(Theme.accent) }

    /// Page paper: the flavour's card colour, the desktop's
    /// `Palette::paper` in crates/krabink/src/theme.rs. Both platforms
    /// clear the ink layer to this, under the text, so ink reads alike
    /// everywhere; the renderer picks the highlighter blend from its
    /// luminance.
    static var paper: UIColor { UIColor(Theme.surface) }
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
