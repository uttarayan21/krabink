// Shared look for the iPad app, mirroring the desktop theme
// (crates/krabink/src/theme.rs): a dark palette with an indigo accent,
// cards on a darker page, roomier padding. The app is dark-only like the
// desktop, so the sketch paper (`UIColor.paper`, the same value as
// `Theme.surface`) never sits on a white page.

import SwiftUI
import UIKit

enum Theme {
    /// Window background.
    static let bg = Color(hex: 0x1111_17)
    /// Note list and toolbars.
    static let sidebar = Color(hex: 0x1719_20)
    /// Cards, editor, inputs. Equal to `UIColor.paper`.
    static let surface = Color(hex: 0x1D20_29)
    /// Hovered or selected rows, code.
    static let surfaceRaised = Color(hex: 0x262A_36)
    /// Hairlines around cards.
    static let border = Color(hex: 0x2B30_3D)
    /// Primary text.
    static let text = Color(hex: 0xE7E9_F0)
    /// Secondary text, captions, hints.
    static let muted = Color(hex: 0x8D94_A8)
    /// Brand accent: buttons, selection, links.
    static let accent = Color(hex: 0x7C8C_FF)
    /// Translucent accent behind the selected note.
    static let accentSoft = Color(hex: 0x2A2F_5A).opacity(0.8)
    static let success = Color(hex: 0x4ADE_80)
    static let warn = Color(hex: 0xFBBF_24)
    static let danger = Color(hex: 0xF871_71)

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

/// UIKit twins for the UIView-backed editor and preview.
extension UIColor {
    static let themeBg = UIColor(Theme.bg)
    static let themeSurface = UIColor(Theme.surface)
    static let themeText = UIColor(Theme.text)
    static let themeMuted = UIColor(Theme.muted)
    static let themeBorder = UIColor(Theme.border)
    static let themeAccent = UIColor(Theme.accent)
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
                    .foregroundStyle(.white)
            }
            .shadow(color: Theme.accent.opacity(0.35), radius: 6, y: 2)
    }
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
