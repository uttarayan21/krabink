//! Settings window: peers and their routes, this device, the synced device
//! registry, this desktop's `pendant://pair` QR for other devices to scan,
//! and a paste field to join another workspace (mirrors the iPad screen).

use bevy::prelude::*;
use bevy_egui::egui;
use pendant_core::{DeviceId, DeviceMeta, PairInfo};
use pendant_local::{PeerKind, PeerState, PeerStatus, PeerTarget, RelayHealth, RelayTarget, Route};

use crate::node::SyncNode;
use crate::sync::{LOCAL_PLATFORM, local_device_name};
use crate::theme;

/// Quiet-zone border around the QR matrix, in modules (spec minimum is 4).
const QUIET_ZONE: usize = 4;

/// After "Leave workspace" the peers stay up this long so the registry
/// removal reaches them before their connections are dropped.
pub const UNPAIR_LINGER: std::time::Duration = std::time::Duration::from_millis(750);

/// Raised when the user joined a workspace from the pairing window; the
/// handler persists it and repoints the node.
#[derive(Message)]
pub struct PairAdopted(pub PairInfo);

pub struct SettingsPlugin;

impl Plugin for SettingsPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<PairAdopted>()
            .add_systems(Update, apply_adopted);
    }
}

/// Persist the adopted coordinates, teach the node the new token, move it
/// to the workspace's relay, dial the pairing node (and replica), and
/// repoint the shared QR. Registration with the new workspace rides the
/// resulting catch-up.
fn apply_adopted(
    mut adopted: MessageReader<PairAdopted>,
    runtime: Res<crate::Runtime>,
    sync: Res<SyncNode>,
    mut settings: ResMut<Settings>,
) {
    let Some(PairAdopted(info)) = adopted.read().last() else {
        return;
    };
    let target = match PeerTarget::from_pair(info, PeerKind::Desktop) {
        Ok(target) => target,
        Err(err) => {
            tracing::error!(err, "pairing uri rejected");
            return;
        }
    };
    let mut peers = vec![target];
    match PeerTarget::replica_from_pair(info) {
        Ok(Some(replica)) => peers.push(replica),
        Ok(None) => {}
        Err(err) => tracing::warn!(err, "ignoring replica in pairing uri"),
    }
    match crate::config::persist_pair(info) {
        // Connect live either way; persistence only affects the next launch.
        Ok(path) => tracing::info!(node = %info.node, path = %path.display(), "pair adopted"),
        Err(err) => tracing::error!(%err, "persisting pairing failed"),
    }
    let node = sync.node.clone();
    let relay = peers[0].relay.clone().map(|url| RelayTarget {
        url,
        token: info.token.clone(),
    });
    node.add_token(info.token.clone());
    runtime.0.block_on(async {
        if let Err(err) = node.set_relay(relay).await {
            tracing::error!(%err, "switching relay failed");
        }
        node.set_peers(peers).await;
    });
    // Our QR keeps pointing at us; the workspace's token, relay and
    // replica become everyone's.
    let own = PairInfo {
        node: settings.info.node.clone(),
        token: info.token.clone(),
        relay: info.relay.clone(),
        addrs: settings.info.addrs.clone(),
        replica: info.replica.clone(),
    };
    settings.adopt(own);
}

/// What the user asked for from the window this frame.
pub enum SettingsAction {
    /// Join the workspace behind a pasted pairing URI.
    Join(PairInfo),
    /// Forget a device (by id) in the synced registry.
    RemoveDevice(DeviceId),
    /// Call this desktop something else, everywhere.
    Rename(String),
    /// Leave the adopted workspace (our row goes, peers are dropped).
    Unpair,
}

/// Read-only snapshot the window renders each frame; gathered by the
/// caller because it spans several ECS resources.
pub struct SettingsView {
    pub peers: Vec<PeerStatus>,
    pub relay: RelayHealth,
    pub mdns_name: Option<String>,
    pub this_device: DeviceId,
    pub devices: Vec<DeviceMeta>,
    pub now_ms: u64,
}

#[derive(Resource)]
pub struct Settings {
    /// What the QR advertises: our node, the shared token, the relay and
    /// replica (when configured) and our direct addresses.
    pub info: PairInfo,
    pub open: bool,
    /// This install's own token: what the QR carries when no workspace is
    /// adopted, and how we tell "paired" from "on our own".
    own_token: String,
    texture: Option<egui::TextureHandle>,
    join_uri: String,
    join_error: bool,
    /// The device-name field's buffer (saved explicitly, not per keystroke).
    name_edit: String,
    /// Device id whose "remove" was clicked once; second click confirms.
    pending_remove: Option<DeviceId>,
    /// "Leave workspace" was clicked once; second click confirms.
    pending_unpair: bool,
}

impl Settings {
    pub fn new(info: PairInfo, own_token: String) -> Self {
        Self {
            info,
            open: false,
            own_token,
            texture: None,
            join_uri: String::new(),
            join_error: false,
            name_edit: local_device_name(),
            pending_remove: None,
            pending_unpair: false,
        }
    }

    /// Adopted a workspace token (as opposed to running on our own)?
    fn paired(&self) -> bool {
        self.info.token != self.own_token
    }

    /// Back to our own token, no relay, no replica: what the QR shows
    /// after leaving a workspace.
    pub fn unpaired_info(&self) -> PairInfo {
        PairInfo {
            node: self.info.node.clone(),
            token: self.own_token.clone(),
            relay: None,
            addrs: self.info.addrs.clone(),
            replica: None,
        }
    }

    /// The endpoint's direct addresses changed: refresh the QR.
    pub fn set_addrs(&mut self, addrs: Vec<String>) {
        if self.info.addrs != addrs {
            self.info.addrs = addrs;
            self.texture = None;
        }
    }

    /// Repoint at a newly joined (or just left) workspace; drops the stale
    /// QR texture.
    pub fn adopt(&mut self, info: PairInfo) {
        self.info = info;
        self.texture = None;
        self.join_uri.clear();
        self.join_error = false;
        self.pending_unpair = false;
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
            self.pending_unpair = false;
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
                        if let Some(asked) = self.sync_section(ui, view) {
                            action = Some(asked);
                        }
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

    /// Peers, relay, this device (with its editable name) and, when a
    /// workspace is adopted, the way out of it.
    fn sync_section(&mut self, ui: &mut egui::Ui, view: &SettingsView) -> Option<SettingsAction> {
        let mut action = None;
        let paired = self.paired();
        let mut pending_unpair = self.pending_unpair;
        theme::section(ui, "Sync", |ui| {
            let peers: Vec<&PeerStatus> = view
                .peers
                .iter()
                .filter(|p| p.kind != PeerKind::Local)
                .collect();
            if peers.is_empty() {
                ui.weak("No peers yet. Pair a device below or join another workspace.");
            }
            egui::Grid::new("peers")
                .num_columns(3)
                .spacing([16.0, 8.0])
                .show(ui, |ui| {
                    for peer in peers {
                        let (dot, label) = match &peer.state {
                            PeerState::Connected {
                                route: Some(Route::Direct(_)),
                            } => (theme::SUCCESS, "direct"),
                            PeerState::Connected {
                                route: Some(Route::Relay(_)),
                            } => (theme::SUCCESS, "via relay"),
                            PeerState::Connected { route: None } => (theme::SUCCESS, "connected"),
                            PeerState::Connecting => (theme::WARN, "connecting…"),
                            PeerState::Fatal { .. } => (theme::DANGER, "rejected"),
                        };
                        theme::status_dot(ui, dot, label);
                        ui.label(match peer.kind {
                            PeerKind::Replica => "cloud replica",
                            PeerKind::Desktop => "desktop",
                            PeerKind::Tablet => "tablet",
                            PeerKind::Local | PeerKind::Unknown => {
                                if peer.inbound {
                                    "device (dialled us)"
                                } else {
                                    "peer"
                                }
                            }
                        });
                        let detail = match &peer.state {
                            PeerState::Connected {
                                route: Some(Route::Direct(addr)),
                            } => addr.to_string(),
                            PeerState::Connected {
                                route: Some(Route::Relay(url)),
                            } => url.to_string(),
                            PeerState::Fatal { message } => message.clone(),
                            _ => peer
                                .id
                                .map(|id| id.fmt_short().to_string())
                                .unwrap_or_default(),
                        };
                        ui.monospace(egui::RichText::new(detail).color(theme::MUTED));
                        ui.end_row();
                    }
                });
            ui.add_space(4.0);
            ui.weak(match (&self.info.relay, &view.relay) {
                (Some(url), health) if health.connected => format!("relay {url}: connected"),
                (Some(url), health) => match &health.error {
                    Some(err) => format!("relay {url}: {err}"),
                    None => format!("relay {url}: connecting…"),
                },
                (None, _) => "no relay: LAN only".to_string(),
            });
            ui.weak(match &view.mdns_name {
                Some(name) => format!("mDNS: {name}"),
                None => "mDNS: off (advertising failed)".to_string(),
            });
            if !self.info.addrs.is_empty() {
                ui.add_space(4.0);
                ui.weak("direct addresses:");
                for addr in &self.info.addrs {
                    ui.monospace(egui::RichText::new(addr).color(theme::MUTED));
                }
            }
            if self.info.relay.is_none() {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "No relay: devices must reach this desktop on the LAN. \
                         Add one with --relay or by joining a workspace to sync across networks.",
                    )
                    .color(theme::WARN),
                );
            }
            if paired {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if pending_unpair {
                        if ui.add(theme::danger_button("confirm leave")).clicked() {
                            action = Some(SettingsAction::Unpair);
                        }
                        if ui.small_button("cancel").clicked() {
                            pending_unpair = false;
                        }
                        ui.weak("Drops this desktop from every device's list and stops syncing.");
                    } else if ui.button("Leave workspace").clicked() {
                        pending_unpair = true;
                    }
                });
                ui.weak(
                    "Notes already synced stay on this desktop; the QR goes back to its own token.",
                );
            }
        });
        self.pending_unpair = pending_unpair && action.is_none();

        let current_name = local_device_name();
        theme::section(ui, "This device", |ui| {
            egui::Grid::new("this-device")
                .num_columns(2)
                .spacing([24.0, 8.0])
                .show(ui, |ui| {
                    ui.weak("name");
                    ui.horizontal(|ui| {
                        let edit = ui.add(
                            egui::TextEdit::singleline(&mut self.name_edit)
                                .hint_text(crate::config::default_device_name())
                                .desired_width(200.0),
                        );
                        let proposed = self.name_edit.trim();
                        let changed = !proposed.is_empty() && proposed != current_name;
                        let submitted =
                            edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if ui
                            .add_enabled(changed, theme::primary_button("Save"))
                            .clicked()
                            || (submitted && changed)
                        {
                            action = Some(SettingsAction::Rename(proposed.to_string()));
                        }
                    });
                    ui.end_row();
                    ui.weak("platform");
                    ui.label(LOCAL_PLATFORM);
                    ui.end_row();
                    ui.weak("id");
                    ui.monospace(
                        egui::RichText::new(view.this_device.to_string()).color(theme::MUTED),
                    );
                    ui.end_row();
                    ui.weak("node");
                    ui.monospace(egui::RichText::new(&self.info.node).color(theme::MUTED));
                    ui.end_row();
                });
            ui.weak("Every paired device shows this name in its list.");
        });
        action
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
            ui.weak(match &self.info.relay {
                Some(relay) => format!(
                    "The device meets this desktop through {relay} and talks to it \
                     directly whenever a path exists (LAN, Tailscale, hole-punched)."
                ),
                None => "No relay configured: the device must be on the same network \
                         (mDNS) or reach one of the addresses below."
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
