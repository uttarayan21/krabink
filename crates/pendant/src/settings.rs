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

/// What the user asked for from the window this frame.
pub enum SettingsAction {
    /// Join the workspace behind a pasted pairing URI.
    Join(PairInfo),
    /// Forget a device (by id) in the synced registry.
    RemoveDevice(String),
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
    /// Device id whose "remove" was clicked once; second click confirms.
    pending_remove: Option<String>,
}

impl Settings {
    pub fn new(info: PairInfo) -> Self {
        Self {
            info,
            open: false,
            texture: None,
            join_uri: String::new(),
            join_error: false,
            pending_remove: None,
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

    /// Show the settings window when toggled open; returns what the user
    /// asked for, if anything.
    pub fn window(&mut self, ctx: &egui::Context, view: &SettingsView) -> Option<SettingsAction> {
        if !self.open {
            self.pending_remove = None;
            return None;
        }
        let texture = self.qr_texture(ctx);
        let mut open = self.open;
        let mut action = None;
        egui::Window::new("settings")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(640.0)
                    .show(ui, |ui| {
                        self.sync_section(ui, view);
                        if let Some(id) = self.devices_section(ui, view) {
                            action = Some(SettingsAction::RemoveDevice(id));
                        }
                        self.pair_section(ui, texture.as_ref());
                        if let Some(info) = self.join_section(ui) {
                            action = Some(SettingsAction::Join(info));
                        }
                    });
            });
        self.open = open;
        action
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

    /// Returns the id of a device the user confirmed removing.
    fn devices_section(&mut self, ui: &mut egui::Ui, view: &SettingsView) -> Option<String> {
        ui.heading("paired devices");
        let others: Vec<&DeviceMeta> = view
            .devices
            .iter()
            .filter(|d| d.id != view.this_device)
            .collect();
        let mut removed = None;
        if others.is_empty() {
            ui.label("none yet — devices appear here once they connect");
            ui.label("to the same workspace.");
        } else {
            // A row vanishing from the registry cancels its pending removal.
            if let Some(pending) = &self.pending_remove
                && !others.iter().any(|d| &d.id == pending)
            {
                self.pending_remove = None;
            }
            egui::Grid::new("devices")
                .num_columns(4)
                .striped(true)
                .show(ui, |ui| {
                    for d in others {
                        ui.label(&d.name);
                        ui.label(&d.platform);
                        ui.weak(format!("seen {}", ago(view.now_ms, d.last_seen_ms)));
                        if self.pending_remove.as_deref() == Some(d.id.as_str()) {
                            ui.horizontal(|ui| {
                                let confirm = egui::Button::new(
                                    egui::RichText::new("confirm remove")
                                        .color(egui::Color32::LIGHT_RED),
                                );
                                if ui.add(confirm).clicked() {
                                    removed = Some(d.id.clone());
                                }
                                if ui.small_button("cancel").clicked() {
                                    self.pending_remove = None;
                                }
                            });
                        } else if ui.small_button("remove").clicked() {
                            self.pending_remove = Some(d.id.clone());
                        }
                        ui.end_row();
                    }
                });
            ui.weak("removing only forgets the row; the device re-appears if");
            ui.weak("it reconnects with the same token.");
        }
        if removed.is_some() {
            self.pending_remove = None;
        }
        ui.separator();
        removed
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
