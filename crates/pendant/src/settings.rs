//! Settings window: per-relay sync state, this device, the synced device
//! registry, this desktop's `pendant://pair` QR for other devices to scan,
//! and a paste field to join another workspace (mirrors the iPad screen).

use bevy::prelude::*;
use bevy_egui::egui;
use pendant_core::{DeviceId, DeviceMeta, PairInfo};

use crate::discovery::PairedDesktop;
use crate::sync::{LOCAL_PLATFORM, LinkKind, LinkStatus, SyncStatus, local_device_name};
use crate::theme;

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
/// workspace's relays, teach the embedded relay the new token, repoint
/// the shared QR, and (when the URI came from a desktop) start looking
/// for that desktop over mDNS. Registration with the new workspace rides
/// the resulting `Connected` effect.
fn apply_adopted(
    mut adopted: MessageReader<PairAdopted>,
    mut commands: Commands,
    runtime: Res<crate::Runtime>,
    relay: Res<crate::relay::EmbeddedRelay>,
    mut transport: ResMut<crate::sync::SyncTransport>,
    mut settings: ResMut<Settings>,
) {
    let Some(PairAdopted(info)) = adopted.read().last() else {
        return;
    };
    match PairedDesktop::from_pair(info) {
        Some(paired) => commands.insert_resource(paired),
        None => commands.remove_resource::<PairedDesktop>(),
    }
    match crate::config::persist_pair(info) {
        // Connect live either way; persistence only affects the next launch.
        Ok(path) => tracing::info!(server = %info.server, path = %path.display(), "pair adopted"),
        Err(err) => tracing::error!(%err, "persisting pairing failed"),
    }
    relay.add_token(&info.token);
    // Desktop-to-desktop takes the preferred direct path + fallback only;
    // the alternates are for mobile clients that hop networks.
    transport.replace_remotes(
        runtime.0.handle(),
        std::iter::once(info.server.clone()).chain(info.fallback.clone()),
        &info.token,
    );
    // Our QR keeps advertising our own relay as the direct path; the
    // workspace we joined becomes the fallback everyone shares.
    settings.adopt(PairInfo {
        server: relay.advertised.clone(),
        token: info.token.clone(),
        fallback: Some(info.fallback.clone().unwrap_or_else(|| info.server.clone())),
        alt: relay.alt.clone(),
        relay_id: Some(transport.device().to_string()),
    });
}

/// What the user asked for from the window this frame.
pub enum SettingsAction {
    /// Join the workspace behind a pasted pairing URI.
    Join(PairInfo),
    /// Forget a device (by id) in the synced registry.
    RemoveDevice(DeviceId),
}

/// Read-only snapshot the window renders each frame; gathered by the
/// caller because it spans several ECS resources.
pub struct SettingsView {
    pub links: Vec<LinkStatus>,
    pub mdns_name: Option<String>,
    /// Device id of the desktop we joined, when there is one.
    pub paired_relay: Option<String>,
    /// Where mDNS last saw that desktop; `None` until found.
    pub discovered: Option<String>,
    pub this_device: DeviceId,
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
    pending_remove: Option<DeviceId>,
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

    /// The joined desktop moved (`from` → `to`): when our QR's fallback
    /// pointed at it, follow it so devices we pair inherit the live path.
    pub fn direct_repointed(&mut self, from: &str, to: &str) {
        if self.info.fallback.as_deref() == Some(from) {
            self.info.fallback = Some(to.to_string());
            self.texture = None;
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
        // The window is not resizable, so it takes its content's size; hand
        // the scroll area a screen-relative budget so long content scrolls.
        let max_height = (ctx.content_rect().height() - 96.0).max(240.0);
        egui::Window::new(egui::RichText::new("Settings").strong())
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .default_width(500.0)
            .default_height(max_height)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(max_height - 56.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.add_space(6.0);
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
        theme::section(ui, "Sync", |ui| {
            egui::Grid::new("links")
                .num_columns(3)
                .spacing([16.0, 8.0])
                .show(ui, |ui| {
                    for link in &view.links {
                        let (dot, label) = match link.status {
                            SyncStatus::Connected => (theme::SUCCESS, "connected"),
                            SyncStatus::Connecting => (theme::WARN, "connecting…"),
                        };
                        theme::status_dot(ui, dot, label);
                        ui.label(match link.kind {
                            LinkKind::Embedded => "this desktop's relay",
                            LinkKind::Remote => "dedicated relay",
                        });
                        ui.monospace(
                            egui::RichText::new(match link.kind {
                                // Show the address peers use, not the loopback one.
                                LinkKind::Embedded => &self.info.server,
                                LinkKind::Remote => &link.server,
                            })
                            .color(theme::MUTED),
                        );
                        ui.end_row();
                    }
                });
            if !self.info.alt.is_empty() {
                ui.add_space(4.0);
                ui.weak("also reachable at:");
                for alt in &self.info.alt {
                    ui.monospace(egui::RichText::new(alt).color(theme::MUTED));
                }
            }
            ui.add_space(4.0);
            ui.weak(match &view.mdns_name {
                Some(name) => format!("mDNS: {name}"),
                None => "mDNS: off (advertising failed)".to_string(),
            });
            if let Some(relay) = &view.paired_relay {
                ui.weak(match &view.discovered {
                    Some(url) => format!("paired desktop {relay}: seen over mDNS at {url}"),
                    None => format!(
                        "paired desktop {relay}: not seen over mDNS yet (using stored address)"
                    ),
                });
            }
            if view.links.len() == 1 {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "No dedicated relay: devices must reach this desktop directly. \
                         Add one with --server or config.toml to sync across networks.",
                    )
                    .color(theme::WARN),
                );
            }
        });

        theme::section(ui, "This device", |ui| {
            egui::Grid::new("this-device")
                .num_columns(2)
                .spacing([24.0, 8.0])
                .show(ui, |ui| {
                    ui.weak("name");
                    ui.label(local_device_name());
                    ui.end_row();
                    ui.weak("platform");
                    ui.label(LOCAL_PLATFORM);
                    ui.end_row();
                    ui.weak("id");
                    ui.monospace(
                        egui::RichText::new(view.this_device.to_string()).color(theme::MUTED),
                    );
                    ui.end_row();
                });
        });
    }

    /// Returns the id of a device the user confirmed removing.
    fn devices_section(&mut self, ui: &mut egui::Ui, view: &SettingsView) -> Option<DeviceId> {
        let others: Vec<&DeviceMeta> = view
            .devices
            .iter()
            .filter(|d| d.id != view.this_device)
            .collect();
        // A row vanishing from the registry cancels its pending removal.
        if let Some(pending) = &self.pending_remove
            && !others.iter().any(|d| d.id == *pending)
        {
            self.pending_remove = None;
        }
        let mut removed = None;
        let mut pending = self.pending_remove;
        theme::section(ui, "Paired devices", |ui| {
            if others.is_empty() {
                ui.weak("None yet. Devices appear here once they connect to the same workspace.");
                return;
            }
            egui::Grid::new("devices")
                .num_columns(4)
                .striped(true)
                .spacing([16.0, 8.0])
                .show(ui, |ui| {
                    for d in others {
                        ui.label(egui::RichText::new(&d.name).strong());
                        ui.label(&d.platform);
                        ui.weak(format!("seen {}", ago(view.now_ms, d.last_seen_ms)));
                        if pending == Some(d.id) {
                            ui.horizontal(|ui| {
                                if ui.add(theme::danger_button("confirm remove")).clicked() {
                                    removed = Some(d.id);
                                }
                                if ui.small_button("cancel").clicked() {
                                    pending = None;
                                }
                            });
                        } else if ui.small_button("remove").clicked() {
                            pending = Some(d.id);
                        }
                        ui.end_row();
                    }
                });
            ui.add_space(4.0);
            ui.weak("Removing only forgets the row; the device re-appears if it reconnects with the same token.");
        });
        self.pending_remove = if removed.is_some() { None } else { pending };
        removed
    }

    fn pair_section(&self, ui: &mut egui::Ui, texture: Option<&egui::TextureHandle>) {
        theme::section(ui, "Pair a device", |ui| {
            ui.label(
                "Scan with the other device's camera (iPad: Settings, then Scan pairing code), \
                 or run: pendant pair '<uri below>'.",
            );
            ui.weak(match &self.info.fallback {
                Some(fallback) => format!(
                    "The device connects straight to this desktop on the LAN and falls back \
                     to {fallback} elsewhere."
                ),
                None => "The device connects straight to this desktop on the LAN \
                         (no fallback relay: it must be on the same network)."
                    .to_string(),
            });
            ui.add_space(10.0);
            let uri = self.info.to_uri();
            match texture {
                Some(texture) => {
                    let side = 6.0 * texture.size()[0] as f32;
                    ui.vertical_centered(|ui| {
                        egui::Frame::new()
                            .fill(egui::Color32::WHITE)
                            .corner_radius(egui::CornerRadius::same(theme::RADIUS))
                            .inner_margin(egui::Margin::same(6))
                            .show(ui, |ui| {
                                ui.image((texture.id(), egui::Vec2::splat(side)));
                            });
                    });
                }
                None => {
                    ui.colored_label(theme::DANGER, "pairing uri too long for a QR");
                }
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Copy URI").clicked() {
                    ui.ctx().copy_text(uri.clone());
                }
                ui.add(
                    egui::Label::new(egui::RichText::new(&uri).monospace().color(theme::MUTED))
                        .truncate(),
                );
            });
        });
    }

    fn join_section(&mut self, ui: &mut egui::Ui) -> Option<PairInfo> {
        let mut joined = None;
        let mut join_error = self.join_error;
        theme::section(ui, "Join another workspace", |ui| {
            ui.weak("Paste a pairing URI from another desktop.");
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let width = (ui.available_width() - 90.0).max(160.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.join_uri)
                        .hint_text("pendant://pair?…")
                        .desired_width(width),
                );
                if ui.add(theme::primary_button("Join")).clicked() {
                    match PairInfo::parse(self.join_uri.trim()) {
                        Some(info) => {
                            join_error = false;
                            joined = Some(info);
                        }
                        None => join_error = true,
                    }
                }
            });
            if join_error {
                ui.colored_label(theme::DANGER, "not a pendant://pair URI");
            }
        });
        self.join_error = join_error;
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
