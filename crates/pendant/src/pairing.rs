//! QR pairing window: renders this client's sync coordinates as a
//! `pendant://pair` QR code another device can scan (or a URI it can paste
//! into `pendant pair <uri>`).

use bevy::prelude::*;
use bevy_egui::egui;
use pendant_core::PairInfo;

/// Quiet-zone border around the QR matrix, in modules (spec minimum is 4).
const QUIET_ZONE: usize = 4;

#[derive(Resource, Default)]
pub struct PairShare {
    /// Absent when running offline — nothing to share.
    pub info: Option<PairInfo>,
    pub open: bool,
    texture: Option<egui::TextureHandle>,
}

impl PairShare {
    pub fn new(info: Option<PairInfo>) -> Self {
        Self {
            info,
            ..Default::default()
        }
    }

    /// Show the pairing window when toggled open. No-op while closed.
    pub fn window(&mut self, ctx: &egui::Context) {
        if !self.open {
            return;
        }
        let Some(info) = self.info.clone() else {
            return;
        };
        let uri = info.to_uri();
        let texture = match &self.texture {
            Some(t) => t.clone(),
            None => match qr_image(&uri) {
                Some(image) => {
                    let t = ctx.load_texture("pair-qr", image, egui::TextureOptions::NEAREST);
                    self.texture = Some(t.clone());
                    t
                }
                None => {
                    tracing::error!("pairing uri does not fit in a QR code");
                    self.open = false;
                    return;
                }
            },
        };

        let mut open = self.open;
        egui::Window::new("pair a device")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label("scan with the other device's camera,");
                ui.label("or run: pendant pair '<uri below>'");
                ui.add_space(8.0);
                let side = 6.0 * texture.size()[0] as f32;
                ui.image((texture.id(), egui::Vec2::splat(side)));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("copy uri").clicked() {
                        ui.ctx().copy_text(uri.clone());
                    }
                    ui.monospace(&uri);
                });
            });
        self.open = open;
    }
}

/// QR matrix for `uri` as a black/white image with a quiet zone.
fn qr_image(uri: &str) -> Option<egui::ColorImage> {
    let code = qrcode::QrCode::new(uri.as_bytes()).ok()?;
    let width = code.width();
    let side = width + 2 * QUIET_ZONE;
    let mut pixels = vec![egui::Color32::WHITE; side * side];
    for (i, color) in code.to_colors().into_iter().enumerate() {
        if color == qrcode::Color::Dark {
            let (x, y) = (i % width + QUIET_ZONE, i / width + QUIET_ZONE);
            pixels[y * side + x] = egui::Color32::BLACK;
        }
    }
    Some(egui::ColorImage {
        size: [side, side],
        pixels,
        ..Default::default()
    })
}
