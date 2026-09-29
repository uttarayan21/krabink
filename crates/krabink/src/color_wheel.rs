//! Colour wheel for the paper picker in Settings: hue by angle (red at
//! three o'clock, running clockwise), saturation growing from the centre
//! out, and a brightness bar beside it. The iPad's `ColorWheel` in
//! ColorWheel.swift uses the same mapping, so a colour sits in the same
//! spot on both.

use std::f32::consts::TAU;

use bevy_egui::egui;
use egui::{Color32, Mesh, Pos2, Rect, Sense, Shape, Stroke, Vec2};

/// Rings from the centre to the rim; saturation steps between them.
const RINGS: u32 = 12;
/// Wedges around the wheel; hue steps between them.
const SEGMENTS: u32 = 96;

/// Hue, saturation and value in `0..=1` over sRGB components (the HSB
/// of `UIColor.getHue`). Kept as the picker's state rather than derived
/// from the colour each frame, so the hue survives dragging to grey or
/// black.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hsv {
    pub h: f32,
    pub s: f32,
    pub v: f32,
}

impl Hsv {
    pub fn to_color(self) -> Color32 {
        let Self { h, s, v } = self;
        let sector = h.rem_euclid(1.0) * 6.0;
        let chroma = v * s;
        let x = chroma * (1.0 - (sector % 2.0 - 1.0).abs());
        let (r, g, b) = match sector.floor() as u8 {
            0 => (chroma, x, 0.0),
            1 => (x, chroma, 0.0),
            2 => (0.0, chroma, x),
            3 => (0.0, x, chroma),
            4 => (x, 0.0, chroma),
            _ => (chroma, 0.0, x),
        };
        let m = v - chroma;
        let byte = |c: f32| ((c + m) * 255.0).round().clamp(0.0, 255.0) as u8;
        Color32::from_rgb(byte(r), byte(g), byte(b))
    }

    pub fn from_color(color: Color32) -> Self {
        let [r, g, b] = [color.r(), color.g(), color.b()].map(|c| f32::from(c) / 255.0);
        let max = r.max(g).max(b);
        let delta = max - r.min(g).min(b);
        let sector = if delta == 0.0 {
            0.0
        } else if max == r {
            ((g - b) / delta).rem_euclid(6.0)
        } else if max == g {
            (b - r) / delta + 2.0
        } else {
            (r - g) / delta + 4.0
        };
        Self {
            h: sector / 6.0,
            s: if max == 0.0 { 0.0 } else { delta / max },
            v: max,
        }
    }
}

/// The wheel and its brightness bar side by side. The response is the
/// union of both: `changed` when `hsv` moved this frame, `drag_stopped` or
/// `clicked` once the user lets go.
pub fn picker(ui: &mut egui::Ui, hsv: &mut Hsv, diameter: f32, border: Color32) -> egui::Response {
    ui.horizontal(|ui| {
        let wheel = wheel(ui, hsv, diameter, border);
        ui.add_space(10.0);
        let bar = value_bar(ui, hsv, Vec2::new(18.0, diameter), border);
        wheel | bar
    })
    .inner
}

/// Hue by angle, saturation by distance from the centre, drawn at
/// `hsv.v`. Pressing or dragging anywhere on it picks.
pub fn wheel(ui: &mut egui::Ui, hsv: &mut Hsv, diameter: f32, border: Color32) -> egui::Response {
    let (rect, mut response) =
        ui.allocate_exact_size(Vec2::splat(diameter), Sense::click_and_drag());
    let center = rect.center();
    let radius = 0.5 * diameter;
    if let Some(pos) = response.interact_pointer_pos() {
        let offset = pos - center;
        let h = (offset.y.atan2(offset.x) / TAU).rem_euclid(1.0);
        let s = (offset.length() / radius).min(1.0);
        if (h, s) != (hsv.h, hsv.s) {
            hsv.h = h;
            hsv.s = s;
            response.mark_changed();
        }
    }
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::ColorButton, ui.is_enabled(), "paper hue")
    });
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.add(Shape::mesh(wheel_mesh(center, radius, hsv.v)));
        painter.circle_stroke(center, radius, Stroke::new(1.0, border));
        let angle = hsv.h * TAU;
        let at = center + radius * hsv.s * Vec2::angled(angle);
        marker(painter, at, hsv.to_color());
    }
    response
}

/// Vertical brightness bar: full value at the top, black at the bottom,
/// in the wheel's current hue and saturation.
pub fn value_bar(ui: &mut egui::Ui, hsv: &mut Hsv, size: Vec2, border: Color32) -> egui::Response {
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click_and_drag());
    if let Some(pos) = response.interact_pointer_pos() {
        let v = 1.0 - ((pos.y - rect.top()) / rect.height()).clamp(0.0, 1.0);
        if v != hsv.v {
            hsv.v = v;
            response.mark_changed();
        }
    }
    response.widget_info(|| {
        egui::WidgetInfo::slider(ui.is_enabled(), f64::from(hsv.v), "paper brightness")
    });
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.add(Shape::mesh(gradient(
            rect,
            Hsv { v: 1.0, ..*hsv }.to_color(),
            Color32::BLACK,
        )));
        painter.rect_stroke(
            rect,
            egui::CornerRadius::ZERO,
            Stroke::new(1.0, border),
            egui::StrokeKind::Outside,
        );
        let y = egui::lerp(rect.bottom()..=rect.top(), hsv.v);
        marker(painter, Pos2::new(rect.center().x, y), hsv.to_color());
    }
    response
}

/// Ring-and-wedge mesh: vertex colours carry hue and saturation, the
/// rasteriser blends between them.
fn wheel_mesh(center: Pos2, radius: f32, v: f32) -> Mesh {
    let mut mesh = Mesh::default();
    mesh.colored_vertex(center, Hsv { h: 0.0, s: 0.0, v }.to_color());
    for ring in 1..=RINGS {
        let s = ring as f32 / RINGS as f32;
        for segment in 0..SEGMENTS {
            let h = segment as f32 / SEGMENTS as f32;
            mesh.colored_vertex(
                center + radius * s * Vec2::angled(h * TAU),
                Hsv { h, s, v }.to_color(),
            );
        }
    }
    // Vertex of `segment` (wrapping) on `ring` (1-based).
    let at = |ring: u32, segment: u32| 1 + (ring - 1) * SEGMENTS + segment % SEGMENTS;
    for segment in 0..SEGMENTS {
        mesh.add_triangle(0, at(1, segment), at(1, segment + 1));
        for ring in 1..RINGS {
            let (a, b) = (at(ring, segment), at(ring, segment + 1));
            let (c, d) = (at(ring + 1, segment), at(ring + 1, segment + 1));
            mesh.add_triangle(a, c, d);
            mesh.add_triangle(a, d, b);
        }
    }
    mesh
}

/// `rect` shaded from `top` to `bottom`.
fn gradient(rect: Rect, top: Color32, bottom: Color32) -> Mesh {
    let mut mesh = Mesh::default();
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.left_bottom(), bottom);
    mesh.colored_vertex(rect.right_bottom(), bottom);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 3, 2);
    mesh
}

/// Picked-colour dot with a white-and-black ring, visible on any colour.
fn marker(painter: &egui::Painter, at: Pos2, fill: Color32) {
    painter.circle(at, 6.0, fill, Stroke::new(2.0, Color32::WHITE));
    painter.circle_stroke(at, 7.5, Stroke::new(1.0, Color32::from_black_alpha(160)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hsv_roundtrips_every_primary_and_grey() {
        for color in [
            Color32::from_rgb(255, 0, 0),
            Color32::from_rgb(0, 255, 0),
            Color32::from_rgb(0, 0, 255),
            Color32::from_rgb(255, 255, 0),
            Color32::from_rgb(0, 255, 255),
            Color32::from_rgb(255, 0, 255),
            Color32::from_rgb(0xcc, 0xd0, 0xda),
            Color32::from_rgb(0x31, 0x32, 0x44),
            Color32::from_rgb(0xf5, 0xef, 0xe0),
            Color32::from_gray(128),
            Color32::BLACK,
            Color32::WHITE,
        ] {
            assert_eq!(Hsv::from_color(color).to_color(), color, "{color:?}");
        }
    }

    #[test]
    fn hue_runs_clockwise_from_red() {
        let red = Hsv::from_color(Color32::from_rgb(255, 0, 0));
        let green = Hsv::from_color(Color32::from_rgb(0, 255, 0));
        let blue = Hsv::from_color(Color32::from_rgb(0, 0, 255));
        assert_eq!(red.h, 0.0);
        assert!((green.h - 1.0 / 3.0).abs() < 1e-6);
        assert!((blue.h - 2.0 / 3.0).abs() < 1e-6);
    }
}
