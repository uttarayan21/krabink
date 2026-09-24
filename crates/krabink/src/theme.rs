//! Visual theme for the desktop app: the four Catppuccin flavours (Latte,
//! Frappé, Macchiato, Mocha) with lavender as the brand accent, roomier
//! spacing, rounded surfaces, and a few shared building blocks (cards,
//! section labels, primary buttons) so every screen looks alike.
//!
//! The active flavour lives in the [`Theme`] resource; every screen reads
//! its [`Palette`] each frame, so switching in Settings restyles the app
//! at once. The iPad mirrors the same palettes in Theme.swift.

use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};
use egui::{Color32, CornerRadius, Margin, Shadow, Stroke, Vec2};

/// One of the Catppuccin flavours: <https://catppuccin.com/palette>.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum Flavor {
    /// The light flavour.
    Latte,
    Frappe,
    Macchiato,
    /// The darkest flavour, the default.
    #[default]
    Mocha,
}

impl Flavor {
    /// Every flavour, lightest first: the order the pickers list them in.
    pub const ALL: [Self; 4] = [Self::Latte, Self::Frappe, Self::Macchiato, Self::Mocha];

    /// Human name for pickers.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Latte => "Latte",
            Self::Frappe => "Frappé",
            Self::Macchiato => "Macchiato",
            Self::Mocha => "Mocha",
        }
    }

    /// Light or dark: which egui base visuals the palette is applied over.
    pub const fn egui_theme(self) -> egui::Theme {
        match self {
            Self::Latte => egui::Theme::Light,
            Self::Frappe | Self::Macchiato | Self::Mocha => egui::Theme::Dark,
        }
    }

    /// The flavour's colours in the app's roles.
    pub const fn palette(self) -> Palette {
        match self {
            Self::Latte => Palette {
                bg: rgb(0xeff1f5),
                sidebar: rgb(0xe6e9ef),
                surface: rgb(0xccd0da),
                surface_raised: rgb(0xbcc0cc),
                surface_pressed: rgb(0xacb0be),
                border: rgb(0xbcc0cc),
                text: rgb(0x4c4f69),
                muted: rgb(0x6c6f85),
                accent: rgb(0x7287fd),
                on_accent: rgb(0xeff1f5),
                success: rgb(0x40a02b),
                warn: rgb(0xdf8e1d),
                danger: rgb(0xd20f39),
            },
            Self::Frappe => Palette {
                bg: rgb(0x303446),
                sidebar: rgb(0x292c3c),
                surface: rgb(0x414559),
                surface_raised: rgb(0x51576d),
                surface_pressed: rgb(0x626880),
                border: rgb(0x51576d),
                text: rgb(0xc6d0f5),
                muted: rgb(0xa5adce),
                accent: rgb(0xbabbf1),
                on_accent: rgb(0x303446),
                success: rgb(0xa6d189),
                warn: rgb(0xe5c890),
                danger: rgb(0xe78284),
            },
            Self::Macchiato => Palette {
                bg: rgb(0x24273a),
                sidebar: rgb(0x1e2030),
                surface: rgb(0x363a4f),
                surface_raised: rgb(0x494d64),
                surface_pressed: rgb(0x5b6078),
                border: rgb(0x494d64),
                text: rgb(0xcad3f5),
                muted: rgb(0xa5adcb),
                accent: rgb(0xb7bdf8),
                on_accent: rgb(0x24273a),
                success: rgb(0xa6da95),
                warn: rgb(0xeed49f),
                danger: rgb(0xed8796),
            },
            Self::Mocha => Palette {
                bg: rgb(0x1e1e2e),
                sidebar: rgb(0x181825),
                surface: rgb(0x313244),
                surface_raised: rgb(0x45475a),
                surface_pressed: rgb(0x585b70),
                border: rgb(0x45475a),
                text: rgb(0xcdd6f4),
                muted: rgb(0xa6adc8),
                accent: rgb(0xb4befe),
                on_accent: rgb(0x1e1e2e),
                success: rgb(0xa6e3a1),
                warn: rgb(0xf9e2af),
                danger: rgb(0xf38ba8),
            },
        }
    }
}

/// `0xRRGGBB` literal to an opaque colour.
const fn rgb(hex: u32) -> Color32 {
    let [_, r, g, b] = hex.to_be_bytes();
    Color32::from_rgb(r, g, b)
}

/// Whether ink draws on light or dark paper; decides the highlighter's
/// blend (multiply on light, screen on dark).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PaperTone {
    Light,
    #[default]
    Dark,
}

/// Every colour the app uses, by role. Catppuccin roles in brackets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Window background, also the bevy clear colour behind egui [base].
    pub bg: Color32,
    /// Library sidebar [mantle].
    pub sidebar: Color32,
    /// Cards, windows, input backgrounds and sketch paper [surface0].
    pub surface: Color32,
    /// Hovered rows, striped table rows, code blocks [surface1].
    pub surface_raised: Color32,
    /// Pressed widgets [surface2].
    pub surface_pressed: Color32,
    /// Hairlines around cards and panels [surface1].
    pub border: Color32,
    /// Primary text [text].
    pub text: Color32,
    /// Secondary text, captions, hints [subtext0].
    pub muted: Color32,
    /// Brand accent: buttons, selection, links [lavender].
    pub accent: Color32,
    /// Text on an accent-filled button [base].
    pub on_accent: Color32,
    pub success: Color32,
    pub warn: Color32,
    pub danger: Color32,
}

/// Corner radius shared by cards, buttons, and inputs.
pub const RADIUS: u8 = 8;
/// Corner radius for floating windows.
pub const WINDOW_RADIUS: u8 = 12;

impl Palette {
    /// Translucent accent for selected rows and text selection.
    pub fn accent_soft(&self) -> Color32 {
        self.accent.gamma_multiply(0.28)
    }

    /// Sketch paper: what a sketch's render target clears to, so ink sits
    /// inline on the preview card with no visible frame. The iPad pins its
    /// canvas to the same colour (`UIColor.paper` in InkRenderer.swift).
    pub const fn paper(&self) -> Color32 {
        self.surface
    }

    /// Light or dark paper, by linear luminance (the iPad applies the same
    /// cut-off in `InkRenderer.isDark`).
    pub fn paper_tone(&self) -> PaperTone {
        let [r, g, b, _] = self.paper().to_normalized_gamma_f32();
        let linear = |c: f32| {
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        let luminance = 0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b);
        if luminance < 0.5 {
            PaperTone::Dark
        } else {
            PaperTone::Light
        }
    }

    /// [`Self::paper`] as a bevy colour for a camera's clear colour.
    pub fn paper_color(&self) -> Color {
        let p = self.paper();
        Color::srgb_u8(p.r(), p.g(), p.b())
    }

    /// [`Self::paper`] as opaque sRGB bytes for a fresh render target.
    pub const fn paper_bytes(&self) -> [u8; 4] {
        let p = self.paper();
        [p.r(), p.g(), p.b(), 255]
    }

    /// Bevy clear colour matching [`Self::bg`] so nothing flashes behind
    /// the panels.
    pub fn clear_color(&self) -> Color {
        Color::srgb_u8(self.bg.r(), self.bg.g(), self.bg.b())
    }

    // ---- egui style ----

    fn style(&self, base: egui::Theme) -> egui::Style {
        use egui::{FontFamily, FontId, TextStyle};

        let mut style = egui::Style {
            visuals: self.visuals(base),
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

    fn visuals(&self, base: egui::Theme) -> egui::Visuals {
        let mut v = match base {
            egui::Theme::Dark => egui::Visuals::dark(),
            egui::Theme::Light => egui::Visuals::light(),
        };
        let radius = CornerRadius::same(RADIUS);
        let shadow = match base {
            egui::Theme::Dark => 140,
            egui::Theme::Light => 50,
        };

        v.panel_fill = self.bg;
        v.window_fill = self.surface;
        v.window_stroke = Stroke::new(1.0, self.border);
        v.window_corner_radius = CornerRadius::same(WINDOW_RADIUS);
        v.window_shadow = Shadow {
            offset: [0, 10],
            blur: 28,
            spread: 0,
            color: Color32::from_black_alpha(shadow),
        };
        v.popup_shadow = Shadow {
            offset: [0, 6],
            blur: 16,
            spread: 0,
            color: Color32::from_black_alpha(shadow * 3 / 4),
        };
        v.menu_corner_radius = CornerRadius::same(WINDOW_RADIUS);

        v.extreme_bg_color = self.bg;
        v.text_edit_bg_color = Some(self.surface);
        v.faint_bg_color = self.surface_raised;
        v.code_bg_color = self.surface_raised;
        v.hyperlink_color = self.accent;
        v.warn_fg_color = self.warn;
        v.error_fg_color = self.danger;
        v.weak_text_color = Some(self.muted);
        v.striped = true;

        v.selection.bg_fill = self.accent_soft();
        v.selection.stroke = Stroke::new(1.0, self.text);
        v.text_cursor.stroke = Stroke::new(2.0, self.accent);

        let w = &mut v.widgets;
        w.noninteractive.bg_fill = self.surface;
        w.noninteractive.weak_bg_fill = self.surface;
        w.noninteractive.bg_stroke = Stroke::new(1.0, self.border);
        w.noninteractive.fg_stroke = Stroke::new(1.0, self.text);
        w.noninteractive.corner_radius = radius;

        w.inactive.bg_fill = self.surface_raised;
        w.inactive.weak_bg_fill = self.surface_raised;
        w.inactive.bg_stroke = Stroke::new(1.0, self.border);
        w.inactive.fg_stroke = Stroke::new(1.0, self.text);
        w.inactive.corner_radius = radius;

        w.hovered.bg_fill = self.surface_pressed;
        w.hovered.weak_bg_fill = self.surface_pressed;
        w.hovered.bg_stroke = Stroke::new(1.0, self.accent.gamma_multiply(0.6));
        w.hovered.fg_stroke = Stroke::new(1.5, self.text);
        w.hovered.corner_radius = radius;
        w.hovered.expansion = 1.0;

        w.active.bg_fill = self.accent_soft();
        w.active.weak_bg_fill = self.accent_soft();
        w.active.bg_stroke = Stroke::new(1.0, self.accent);
        w.active.fg_stroke = Stroke::new(2.0, self.text);
        w.active.corner_radius = radius;
        w.active.expansion = 1.0;

        w.open.bg_fill = self.surface_pressed;
        w.open.weak_bg_fill = self.surface_pressed;
        w.open.bg_stroke = Stroke::new(1.0, self.border);
        w.open.fg_stroke = Stroke::new(1.0, self.text);
        w.open.corner_radius = radius;

        v
    }

    // ---- building blocks ----

    /// A raised, bordered surface with comfortable padding.
    pub fn card(&self) -> egui::Frame {
        egui::Frame::new()
            .fill(self.surface)
            .stroke(Stroke::new(1.0, self.border))
            .corner_radius(CornerRadius::same(RADIUS + 2))
            .inner_margin(Margin::same(14))
    }

    /// Small uppercase caption used above lists and settings sections.
    pub fn caption(&self, ui: &mut egui::Ui, text: &str) {
        ui.label(
            egui::RichText::new(text.to_uppercase())
                .small()
                .strong()
                .color(self.muted),
        );
    }

    /// Section: caption, then the body inside a [`Self::card`].
    pub fn section<R>(
        &self,
        ui: &mut egui::Ui,
        title: &str,
        body: impl FnOnce(&mut egui::Ui) -> R,
    ) -> R {
        self.caption(ui, title);
        ui.add_space(4.0);
        let out = self.card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            body(ui)
        });
        ui.add_space(14.0);
        out.inner
    }

    /// Filled accent button; the one call-to-action per screen.
    pub fn primary_button(&self, text: &str) -> egui::Button<'static> {
        egui::Button::new(
            egui::RichText::new(text.to_owned())
                .strong()
                .color(self.on_accent),
        )
        .fill(self.accent)
        .stroke(Stroke::NONE)
        .corner_radius(CornerRadius::same(RADIUS))
    }

    /// Outlined button for destructive confirmations.
    pub fn danger_button(&self, text: &str) -> egui::Button<'static> {
        egui::Button::new(egui::RichText::new(text.to_owned()).color(self.danger))
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::new(1.0, self.danger))
            .corner_radius(CornerRadius::same(RADIUS))
    }

    /// Coloured status dot followed by a label.
    pub fn status_dot(&self, ui: &mut egui::Ui, color: Color32, label: &str) {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), 4.0, color);
            ui.label(egui::RichText::new(label).color(self.text));
        });
    }
}

/// The active flavour. Change it with [`Theme::set_flavor`]; the
/// [`apply`] system restyles egui and the bevy clear colour on the next
/// frame, and the sketch module recolours its paper.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    flavor: Flavor,
    palette: Palette,
}

impl Theme {
    pub const fn new(flavor: Flavor) -> Self {
        Self {
            flavor,
            palette: flavor.palette(),
        }
    }

    pub const fn flavor(&self) -> Flavor {
        self.flavor
    }

    pub const fn palette(&self) -> &Palette {
        &self.palette
    }

    pub fn set_flavor(&mut self, flavor: Flavor) {
        *self = Self::new(flavor);
    }

    /// The window's clear colour for this theme.
    pub fn clear_color(&self) -> ClearColor {
        ClearColor(self.palette.clear_color())
    }
}

/// Bevy plugin: keeps egui's style and the clear colour in step with the
/// [`Theme`] resource, which the app inserts (with a matching
/// [`ClearColor`]) before running. The style lands on the first frame the
/// primary context exists; egui contexts are not available at `Startup`.
pub struct ThemePlugin;

impl Plugin for ThemePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(bevy_egui::EguiPrimaryContextPass, apply);
    }
}

/// Restyle when the theme changed (including the frame it was inserted).
pub fn apply(
    mut contexts: EguiContexts,
    theme: Res<Theme>,
    mut clear: ResMut<ClearColor>,
    mut applied: Local<Option<Flavor>>,
) -> Result {
    if *applied == Some(theme.flavor()) {
        return Ok(());
    }
    let ctx = contexts.ctx_mut()?;
    install(ctx, &theme);
    *clear = theme.clear_color();
    *applied = Some(theme.flavor());
    Ok(())
}

/// Build the style and hand it to the context.
pub fn install(ctx: &egui::Context, theme: &Theme) {
    let base = theme.flavor().egui_theme();
    ctx.set_theme(match base {
        egui::Theme::Dark => egui::ThemePreference::Dark,
        egui::Theme::Light => egui::ThemePreference::Light,
    });
    ctx.set_style_of(base, theme.palette().style(base));
}
