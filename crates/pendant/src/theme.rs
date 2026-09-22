//! Visual theme for the desktop app: a dark palette with an indigo accent,
//! roomier spacing, rounded surfaces, and a few shared building blocks
//! (cards, section labels, primary buttons) so every screen looks alike.

use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};
use egui::{Color32, CornerRadius, Margin, Shadow, Stroke, Vec2};

// ---- palette ----

/// Window background (also the bevy clear colour behind egui).
pub const BG: Color32 = Color32::from_rgb(0x11, 0x12, 0x17);
/// Library sidebar.
pub const SIDEBAR: Color32 = Color32::from_rgb(0x17, 0x19, 0x20);
/// Cards, windows, and input backgrounds.
pub const SURFACE: Color32 = Color32::from_rgb(0x1d, 0x20, 0x29);
/// Hovered rows, striped table rows, code blocks.
pub const SURFACE_RAISED: Color32 = Color32::from_rgb(0x26, 0x2a, 0x36);
/// Pressed widgets.
pub const SURFACE_PRESSED: Color32 = Color32::from_rgb(0x2f, 0x34, 0x42);
/// Hairlines around cards and panels.
pub const BORDER: Color32 = Color32::from_rgb(0x2b, 0x30, 0x3d);
/// Primary text.
pub const TEXT: Color32 = Color32::from_rgb(0xe7, 0xe9, 0xf0);
/// Secondary text, captions, hints.
pub const MUTED: Color32 = Color32::from_rgb(0x8d, 0x94, 0xa8);
/// Brand accent: buttons, selection, links.
pub const ACCENT: Color32 = Color32::from_rgb(0x7c, 0x8c, 0xff);
/// Translucent accent for selected rows and text selection.
pub const ACCENT_SOFT: Color32 = Color32::from_rgba_premultiplied(0x2a, 0x2f, 0x5a, 0xcc);
pub const SUCCESS: Color32 = Color32::from_rgb(0x4a, 0xde, 0x80);
pub const WARN: Color32 = Color32::from_rgb(0xfb, 0xbf, 0x24);
pub const DANGER: Color32 = Color32::from_rgb(0xf8, 0x71, 0x71);

/// Sketch paper: what a sketch's render target clears to, so ink sits
/// inline on the preview card with no visible frame. The iPad pins its
/// canvas to the same value (`UIColor.paper` in InkRenderer.swift); keep
/// them equal so a sketch reads alike on both devices.
pub const PAPER: Color32 = SURFACE;
/// Dark paper flips the highlighter from multiply to screen (as on the
/// iPad), so it lightens over what is under it instead of vanishing.
pub const PAPER_IS_DARK: bool = true;

/// [`PAPER`] as a bevy colour for a camera's clear colour.
pub fn paper_color() -> Color {
    Color::srgb_u8(PAPER.r(), PAPER.g(), PAPER.b())
}

/// [`PAPER`] as opaque sRGB bytes for a freshly allocated render target.
pub const fn paper_bytes() -> [u8; 4] {
    [PAPER.r(), PAPER.g(), PAPER.b(), 255]
}

/// Corner radius shared by cards, buttons, and inputs.
pub const RADIUS: u8 = 8;
/// Corner radius for floating windows.
pub const WINDOW_RADIUS: u8 = 12;

/// Bevy clear colour matching [`BG`] so nothing flashes behind the panels.
pub fn clear_color() -> Color {
    Color::srgb_u8(BG.r(), BG.g(), BG.b())
}

/// Bevy plugin: installs the egui style on the first frame the primary
/// context exists (egui contexts are not available at `Startup`).
pub struct ThemePlugin;

impl Plugin for ThemePlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(clear_color_resource())
            .add_systems(bevy_egui::EguiPrimaryContextPass, install_once);
    }
}

fn clear_color_resource() -> ClearColor {
    ClearColor(clear_color())
}

pub fn install_once(mut contexts: EguiContexts, mut installed: Local<bool>) -> Result {
    if *installed {
        return Ok(());
    }
    let ctx = contexts.ctx_mut()?;
    install(ctx);
    *installed = true;
    Ok(())
}

/// Build the style and hand it to the context (dark theme only).
pub fn install(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.set_style_of(egui::Theme::Dark, style());
}

fn style() -> egui::Style {
    use egui::{FontFamily, FontId, TextStyle};

    let mut style = egui::Style {
        visuals: visuals(),
        ..Default::default()
    };

    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(12.0, 6.0);
    style.spacing.window_margin = Margin::same(18);
    style.spacing.menu_margin = Margin::same(8);
    style.spacing.interact_size = Vec2::new(40.0, 26.0);
    style.spacing.indent = 18.0;
    style.spacing.text_edit_width = 320.0;
    style.spacing.scroll.bar_width = 6.0;
    style.spacing.scroll.floating = true;

    style.text_styles = [
        (
            TextStyle::Heading,
            FontId::new(20.0, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(14.5, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(14.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Small,
            FontId::new(11.5, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(13.5, FontFamily::Monospace),
        ),
    ]
    .into();
    style
}

fn visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    let radius = CornerRadius::same(RADIUS);

    v.panel_fill = BG;
    v.window_fill = SURFACE;
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.window_corner_radius = CornerRadius::same(WINDOW_RADIUS);
    v.window_shadow = Shadow {
        offset: [0, 10],
        blur: 28,
        spread: 0,
        color: Color32::from_black_alpha(140),
    };
    v.popup_shadow = Shadow {
        offset: [0, 6],
        blur: 16,
        spread: 0,
        color: Color32::from_black_alpha(110),
    };
    v.menu_corner_radius = CornerRadius::same(WINDOW_RADIUS);

    v.extreme_bg_color = BG;
    v.text_edit_bg_color = Some(SURFACE);
    v.faint_bg_color = SURFACE_RAISED;
    v.code_bg_color = SURFACE_RAISED;
    v.hyperlink_color = ACCENT;
    v.warn_fg_color = WARN;
    v.error_fg_color = DANGER;
    v.weak_text_color = Some(MUTED);
    v.striped = true;

    v.selection.bg_fill = ACCENT_SOFT;
    v.selection.stroke = Stroke::new(1.0, TEXT);
    v.text_cursor.stroke = Stroke::new(2.0, ACCENT);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = SURFACE;
    w.noninteractive.weak_bg_fill = SURFACE;
    w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    w.noninteractive.corner_radius = radius;

    w.inactive.bg_fill = SURFACE_RAISED;
    w.inactive.weak_bg_fill = SURFACE_RAISED;
    w.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    w.inactive.corner_radius = radius;

    w.hovered.bg_fill = SURFACE_PRESSED;
    w.hovered.weak_bg_fill = SURFACE_PRESSED;
    w.hovered.bg_stroke = Stroke::new(1.0, ACCENT.gamma_multiply(0.6));
    w.hovered.fg_stroke = Stroke::new(1.5, Color32::WHITE);
    w.hovered.corner_radius = radius;
    w.hovered.expansion = 1.0;

    w.active.bg_fill = ACCENT_SOFT;
    w.active.weak_bg_fill = ACCENT_SOFT;
    w.active.bg_stroke = Stroke::new(1.0, ACCENT);
    w.active.fg_stroke = Stroke::new(2.0, Color32::WHITE);
    w.active.corner_radius = radius;
    w.active.expansion = 1.0;

    w.open.bg_fill = SURFACE_PRESSED;
    w.open.weak_bg_fill = SURFACE_PRESSED;
    w.open.bg_stroke = Stroke::new(1.0, BORDER);
    w.open.fg_stroke = Stroke::new(1.0, TEXT);
    w.open.corner_radius = radius;

    v
}

// ---- building blocks ----

/// A raised, bordered surface with comfortable padding.
pub fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(RADIUS + 2))
        .inner_margin(Margin::same(14))
}

/// Small uppercase caption used above lists and settings sections.
pub fn caption(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text.to_uppercase())
            .small()
            .strong()
            .color(MUTED),
    );
}

/// Section: caption, then the body inside a [`card`].
pub fn section<R>(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    caption(ui, title);
    ui.add_space(4.0);
    let out = card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        body(ui)
    });
    ui.add_space(14.0);
    out.inner
}

/// Filled accent button; the one call-to-action per screen.
pub fn primary_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(
        egui::RichText::new(text.to_owned())
            .strong()
            .color(Color32::WHITE),
    )
    .fill(ACCENT)
    .stroke(Stroke::NONE)
    .corner_radius(CornerRadius::same(RADIUS))
}

/// Outlined button for destructive confirmations.
pub fn danger_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text.to_owned()).color(DANGER))
        .fill(Color32::TRANSPARENT)
        .stroke(Stroke::new(1.0, DANGER))
        .corner_radius(CornerRadius::same(RADIUS))
}

/// Coloured status dot followed by a label.
pub fn status_dot(ui: &mut egui::Ui, color: Color32, label: &str) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.0, color);
        ui.label(label);
    });
}
