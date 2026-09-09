//! Settings window: per-relay sync state, this device, the synced device
//! registry, this desktop's `pendant://pair` QR for other devices to scan,
//! and a paste field to join another workspace (mirrors the iPad screen).

use bevy::prelude::*;
use bevy_egui::egui;
use pendant_core::{DeviceMeta, PairInfo};

use crate::sync::{LOCAL_PLATFORM, LinkKind, LinkStatus, SyncStatus, local_device_name};

/// Quiet-zone border around the QR matrix, in modules (spec minimum is 4).
const QUIET_ZONE: usize = 4;

/// Raised when the user joined a workspace from the pairing window; the
/// handler persists it and swaps the live transport.
#[derive(Message)]
pub struct PairAdopted(pub PairInfo);

pub struct SettingsPlugin;

impl Plugin for SettingsPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<PairAdopted>()
            .add_systems(Update, apply_adopted);
    }
}

/// Persist the adopted coordinates, swap the remote links to the new
/// workspace's relays, teach the embedded relay the new token, and repoint
/// the shared QR. Registration with the new workspace rides the resulting
/// `Connected` effect.
fn apply_adopted(
    mut adopted: MessageReader<PairAdopted>,
    runtime: Res<crate::Runtime>,
    relay: Res<crate::relay::EmbeddedRelay>,
    mut transport: ResMut<crate::sync::SyncTransport>,
    mut settings: ResMut<Settings>,
) {
    let Some(PairAdopted(info)) = adopted.read().last() else {
        return;
    };
    match crate::config::persist_pair(info) {
        // Connect live either way; persistence only affects the next launch.
        Ok(path) => tracing::info!(server = %info.server, path = %path.display(), "pair adopted"),
        Err(err) => tracing::error!(%err, "persisting pairing failed"),
    }
    relay.add_token(&info.token);
    transport.replace_remotes(
        runtime.0.handle(),
        info.endpoints().into_iter().map(str::to_string),
        &info.token,
    );
    // Our QR keeps advertising our own relay as the direct path; the
    // workspace we joined becomes the fallback everyone shares.
    settings.adopt(PairInfo {
        server: relay.advertised.clone(),
        token: info.token.clone(),
        fallback: Some(info.fallback.clone().unwrap_or_else(|| info.server.clone())),
    });
}

/// Read-only snapshot the window renders each frame; gathered by the
/// caller because it spans several ECS resources.
pub struct SettingsView {
    pub links: Vec<LinkStatus>,
    pub this_device: String,
    pub devices: Vec<DeviceMeta>,
    pub now_ms: u64,
}

#[derive(Resource)]
pub struct Settings {
    /// What the QR advertises: our embedded relay + the shared token, plus
    /// the dedicated relay as fallback when one is configured.
    pub info: PairInfo,
    pub open: bool,
    texture: Option<egui::TextureHandle>,
    join_uri: String,
    join_error: bool,
}

impl Settings {
    pub fn new(info: PairInfo) -> Self {
        Self {
            info,
            open: false,
            texture: None,
            join_uri: String::new(),
            join_error: false,
        }
    }

    /// Repoint at a newly joined workspace (drops the stale QR texture).
    fn adopt(&mut self, info: PairInfo) {
        self.info = info;
        self.texture = None;
        self.join_uri.clear();
        self.join_error = false;
    }

    fn qr_texture(&mut self, ctx: &egui::Context) -> Option<egui::TextureHandle> {
        if let Some(t) = &self.texture {
            return Some(t.clone());
        }
        let image = qr_image(&self.info.to_uri()).or_else(|| {
            tracing::error!("pairing uri does not fit in a QR code");
            None
        })?;
        let t = ctx.load_texture("pair-qr", image, egui::TextureOptions::NEAREST);
        self.texture = Some(t.clone());
        Some(t)
    }

    /// Show the settings window when toggled open; returns the parsed info
    /// when the user submits a URI to join.
    pub fn window(&mut self, ctx: &egui::Context, view: &SettingsView) -> Option<PairInfo> {
        if !self.open {
            return None;
        }
        let texture = self.qr_texture(ctx);
        let mut open = self.open;
        let mut joined = None;
        egui::Window::new("settings")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(640.0)
                    .show(ui, |ui| {
                        self.sync_section(ui, view);
                        self.devices_section(ui, view);
                        self.pair_section(ui, texture.as_ref());
                        joined = self.join_section(ui);
                    });
            });
        self.open = open;
        joined
    }

    fn sync_section(&self, ui: &mut egui::Ui, view: &SettingsView) {
        ui.heading("sync");
        egui::Grid::new("links").num_columns(3).show(ui, |ui| {
            for link in &view.links {
                let (dot, label) = match link.status {
                    SyncStatus::Connected => (egui::Color32::LIGHT_GREEN, "connected"),
                    SyncStatus::Connecting => (egui::Color32::YELLOW, "connecting…"),
                };
                ui.colored_label(dot, "●");
                ui.label(match link.kind {
                    LinkKind::Embedded => "this desktop's relay",
                    LinkKind::Remote => "dedicated relay",
                });
                ui.horizontal(|ui| {
                    ui.label(label);
                    ui.monospace(match link.kind {
                        // Show the address peers use, not the loopback one.
                        LinkKind::Embedded => &self.info.server,
                        LinkKind::Remote => &link.server,
                    });
                });
                ui.end_row();
            }
        });
        if view.links.len() == 1 {
            ui.weak("no dedicated relay: devices must reach this desktop directly.");
            ui.weak("add one with --server / config.toml to sync across networks.");
        }
        ui.add_space(6.0);
        ui.heading("this device");
        egui::Grid::new("this-device")
            .num_columns(2)
            .show(ui, |ui| {
                ui.label("name");
                ui.label(local_device_name());
                ui.end_row();
                ui.label("platform");
                ui.label(LOCAL_PLATFORM);
                ui.end_row();
                ui.label("id");
                ui.monospace(&view.this_device);
                ui.end_row();
            });
        ui.separator();
    }

    fn devices_section(&self, ui: &mut egui::Ui, view: &SettingsView) {
        ui.heading("paired devices");
        let others: Vec<&DeviceMeta> = view
            .devices
            .iter()
            .filter(|d| d.id != view.this_device)
            .collect();
        if others.is_empty() {
            ui.label("none yet — devices appear here once they connect");
            ui.label("to the same workspace.");
        } else {
            egui::Grid::new("devices")
                .num_columns(3)
                .striped(true)
                .show(ui, |ui| {
                    for d in others {
                        ui.label(&d.name);
                        ui.label(&d.platform);
                        ui.weak(format!("seen {}", ago(view.now_ms, d.last_seen_ms)));
                        ui.end_row();
                    }
                });
        }
        ui.separator();
    }

    fn pair_section(&self, ui: &mut egui::Ui, texture: Option<&egui::TextureHandle>) {
        ui.heading("pair a device");
        ui.label("scan with the other device's camera (iPad: settings →");
        ui.label("scan pairing code), or run: pendant pair '<uri below>'");
        ui.label("the device connects straight to this desktop on the LAN");
        match &self.info.fallback {
            Some(fallback) => ui.label(format!("and falls back to {fallback} elsewhere.")),
            None => ui.label("(no fallback relay: it must be on the same network)."),
        };
        ui.add_space(8.0);
        let uri = self.info.to_uri();
        match texture {
            Some(texture) => {
                let side = 6.0 * texture.size()[0] as f32;
                ui.image((texture.id(), egui::Vec2::splat(side)));
            }
            None => {
                ui.colored_label(egui::Color32::LIGHT_RED, "pairing uri too long for a QR");
            }
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("copy uri").clicked() {
                ui.ctx().copy_text(uri.clone());
            }
            ui.monospace(&uri);
        });
        ui.separator();
    }

    fn join_section(&mut self, ui: &mut egui::Ui) -> Option<PairInfo> {
        let mut joined = None;
        ui.heading("join another workspace");
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.join_uri)
                    .hint_text("pendant://pair?…")
                    .desired_width(320.0),
            );
            if ui.button("join").clicked() {
                match PairInfo::parse(self.join_uri.trim()) {
                    Some(info) => {
                        self.join_error = false;
                        joined = Some(info);
                    }
                    None => self.join_error = true,
                }
            }
        });
        if self.join_error {
            ui.colored_label(egui::Color32::LIGHT_RED, "not a pendant://pair URI");
        }
        joined
    }
}

/// Human "how long ago" for a unix-millisecond timestamp.
fn ago(now_ms: u64, then_ms: u64) -> String {
    let secs = now_ms.saturating_sub(then_ms) / 1000;
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
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
