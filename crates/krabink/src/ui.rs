//! Editor UI: library sidebar and one styled markdown editor bound to the
//! CRDT via prefix/suffix diffing. The source is styled in place (headings
//! large, markers dimmed, lists indented, code monospace); a toggle swaps
//! the editor for the reading view: the same text laid out the same way
//! with its markers hidden (`krabink_core::preview_text`), read-only. The
//! note's page ink renders under the text in both: each frame the editor
//! publishes a [`PageLayout`] (where every anchored element's line sits in
//! the galley, and which part of the galley is on screen) and paints the
//! off-screen page texture the sketch module renders from it. In the
//! reading view anchors resolve through the display's source map, so ink
//! stays on its line.

use std::collections::HashMap;
use std::sync::Arc;

use bevy::prelude::*;
use bevy_egui::{EguiContexts, EguiPrimaryContextPass, egui};
use egui::text::{ByteIndex, CCursor, LayoutJob, LayoutSection, TextFormat};
use krabink_core::{
    Anchor, DocKey, ElementId, NoteDoc, NoteId, StyleKind, StyleRun, preview_text, style_runs,
};

use crate::docs::Docs;
use crate::settings::Settings;
use crate::sketch::PageTexture;
use crate::sync::SyncTransport;
use crate::sync::{LocalCommit, SubscribeNeeded};
use crate::theme::{self, Palette, Theme};

/// Body font size. Anchor space is defined at this size on every platform
/// (the iPad pins its text view to 16 pt too), so ink lines up.
pub const BODY_SIZE: f32 = 16.0;
/// Heading scale per level, 1..=6.
const HEADING_SCALE: [f32; 6] = [1.6, 1.4, 1.25, 1.1, 1.0, 1.0];
/// Indent per list nesting level.
const LIST_INDENT: f32 = 18.0;
/// Scroll room below the last line, as a fraction of the viewport, so ink
/// drawn under it stays reachable (the iPad insets its text view alike).
const TAIL_FRACTION: f32 = 0.6;

/// When set, the newest note auto-opens as the library changes (replay rig).
#[derive(Resource, Default)]
pub struct FollowLatest(pub bool);

#[derive(Resource, Default)]
pub struct EditorState {
    pub open: Option<NoteId>,
    /// The TextEdit's backing buffer.
    pub buffer: String,
    /// Buffer contents as of the last reconciliation with the doc.
    last: String,
    /// Set by the sync layer when remote changes may have landed.
    pub remote_dirty: bool,
    /// Show the reading view (markers hidden, read-only) instead of the
    /// editor.
    pub preview: bool,
    /// Style runs of the buffer, reparsed only when the text changes.
    styled: StyledCache,
    /// The reading view of the buffer, rebuilt only when the text changes.
    previewed: PreviewCache,
}

#[derive(Default)]
struct StyledCache {
    for_text: String,
    runs: Vec<StyleRun>,
}

/// `preview_text` of the buffer: the display text (held mutably for the
/// read-only `TextEdit`), its runs and its source map.
#[derive(Default)]
struct PreviewCache {
    for_text: String,
    display: String,
    runs: Vec<StyleRun>,
    source_of: Arc<Vec<usize>>,
}

impl PreviewCache {
    fn refresh(&mut self, text: &str) {
        if self.for_text != text {
            let preview = preview_text(text);
            self.for_text = text.to_owned();
            self.display = preview.text;
            self.runs = preview.runs;
            self.source_of = Arc::new(preview.source_of);
        }
    }
}

impl StyledCache {
    fn runs_for(&mut self, text: &str) -> &[StyleRun] {
        if self.for_text != text {
            self.runs = style_runs(text);
            self.for_text = text.to_owned();
        }
        &self.runs
    }
}

/// Where the open note's text sits this frame, for the page ink scene.
/// Points are in galley space: the galley's top-left is the origin, y grows
/// down, one unit is one logical point.
#[derive(Resource)]
pub struct PageLayout {
    pub note: Option<NoteId>,
    /// `NoteDoc::version` the origins were resolved against.
    doc_version: Vec<u8>,
    galley_size: egui::Vec2,
    /// The laid-out text, for resolving anchors that are not committed
    /// elements (wet strokes, pointers).
    pub galley: Option<Arc<egui::Galley>>,
    /// Anchor-space origin of every committed page element.
    pub origins: HashMap<ElementId, egui::Vec2>,
    /// The part of the galley that is on screen.
    pub window: egui::Rect,
    /// Pixels per point of the egui context.
    pub scale: f32,
    /// Bumped whenever `origins` or the galley geometry changed.
    pub generation: u64,
    /// In the reading view: for each galley char, the source char it came
    /// from (`PreviewText::source_of`). `None` while editing (identity).
    pub source_of: Option<Arc<Vec<usize>>>,
}

impl Default for PageLayout {
    fn default() -> Self {
        Self {
            note: None,
            doc_version: Vec::new(),
            galley_size: egui::Vec2::ZERO,
            galley: None,
            origins: HashMap::new(),
            window: egui::Rect::ZERO,
            scale: 1.0,
            generation: 0,
            source_of: None,
        }
    }
}

impl PageLayout {
    /// Publish this frame's galley; re-resolve the anchors when the doc or
    /// the text geometry changed.
    fn update(
        &mut self,
        id: NoteId,
        note: &NoteDoc,
        galley: Arc<egui::Galley>,
        window: egui::Rect,
        scale: f32,
        source_of: Option<Arc<Vec<usize>>>,
    ) {
        let version = note.version();
        let size = galley.size();
        let same_map = match (&self.source_of, &source_of) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        };
        if self.note != Some(id)
            || self.doc_version != version
            || self.galley_size != size
            || !same_map
        {
            self.note = Some(id);
            self.doc_version = version;
            self.galley_size = size;
            self.source_of = source_of;
            let map = self.source_of.as_deref().map(Vec::as_slice);
            self.origins = note
                .page_elements()
                .iter()
                .map(|el| {
                    (
                        el.element.id(),
                        resolve_origin(&galley, note, &el.anchor, map),
                    )
                })
                .collect();
            self.generation += 1;
        }
        self.galley = Some(galley);
        self.window = window;
        self.scale = scale;
    }

    /// The galley's source map, for resolving anchors not in `origins`.
    pub fn source_map(&self) -> Option<&[usize]> {
        self.source_of.as_deref().map(Vec::as_slice)
    }
}

/// Anchor-space origin of `anchor` in galley space: the top of the row its
/// line starts on. `source_of` maps galley chars to source chars when the
/// galley is the reading view (`None`: identity). Unresolvable anchors sit
/// below the last row.
pub fn resolve_origin(
    galley: &egui::Galley,
    note: &NoteDoc,
    anchor: &Anchor,
    source_of: Option<&[usize]>,
) -> egui::Vec2 {
    match note.resolve_anchor(anchor) {
        Some(index) => {
            let index = match source_of {
                Some(map) => map.partition_point(|&s| s < index),
                None => index,
            };
            let row = galley.pos_from_cursor(CCursor::new(index));
            egui::vec2(0.0, row.min.y)
        }
        None => egui::vec2(0.0, galley.rect.max.y),
    }
}

pub struct EditorUiPlugin;

impl Plugin for EditorUiPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<EditorState>()
            .init_resource::<FollowLatest>()
            .init_resource::<PageLayout>()
            // Theme first so the very first frame already renders styled.
            .add_systems(EguiPrimaryContextPass, editor_ui.after(theme::apply));
    }
}

/// Char-level (prefix, deleted, inserted) between two strings.
fn splice_of(old: &str, new: &str) -> Option<(usize, usize, String)> {
    if old == new {
        return None;
    }
    let old: Vec<char> = old.chars().collect();
    let new: Vec<char> = new.chars().collect();
    let prefix = old
        .iter()
        .zip(new.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    Some((
        prefix,
        old.len() - prefix - suffix,
        new[prefix..new.len() - suffix].iter().collect(),
    ))
}

#[expect(
    clippy::too_many_arguments,
    reason = "bevy system; each param is a distinct ECS resource"
)]
fn editor_ui(
    mut contexts: EguiContexts,
    mut docs: ResMut<Docs>,
    mut editor: ResMut<EditorState>,
    mut layout: ResMut<PageLayout>,
    page_texture: Res<PageTexture>,
    mut commits: MessageWriter<LocalCommit>,
    mut subscribes: MessageWriter<SubscribeNeeded>,
    mut settings: ResMut<Settings>,
    transport: Res<SyncTransport>,
    mut sync: ResMut<crate::node::SyncNode>,
    runtime: Res<crate::Runtime>,
    mut adopted: MessageWriter<crate::settings::PairAdopted>,
    follow: Res<FollowLatest>,
    mut theme: ResMut<Theme>,
) -> Result {
    let ctx = contexts.ctx_mut()?;
    let scale = ctx.pixels_per_point();
    // Copied out: the resource is only written back on a theme action, so
    // change detection stays quiet on ordinary frames.
    let palette = *theme.palette();
    let view = crate::settings::SettingsView {
        peers: sync.node.peers(),
        relay: sync.node.relay_health(),
        mdns_name: sync.mdns_name().map(str::to_string),
        this_device: transport.device(),
        devices: docs.workspace.devices(),
        now_ms: crate::docs::now_ms(),
        flavor: theme.flavor(),
        palette,
    };
    match settings.window(ctx, &view) {
        Some(crate::settings::SettingsAction::Join(info)) => {
            adopted.write(crate::settings::PairAdopted(info));
        }
        Some(crate::settings::SettingsAction::RemoveDevice(id)) => {
            // Sever the live link both ways: the peer hears the unpair and
            // forgets this desktop; we drop its row from the registry.
            sync.unpair(&runtime.0, id);
            match docs.remove_device(id) {
                Ok(payload) if !payload.is_empty() => {
                    commits.write(LocalCommit {
                        doc: DocKey::WORKSPACE,
                        payload,
                    });
                }
                Ok(_) => {}
                Err(err) => tracing::error!(%err, %id, "removing device failed"),
            }
        }
        Some(crate::settings::SettingsAction::Rename(name)) => {
            crate::sync::set_local_device_name(name.clone());
            if let Err(err) = crate::config::persist_device_name(&name) {
                tracing::error!(%err, "persisting device name failed");
            }
            sync.readvertise(&runtime.0, &name);
            match docs.register_device(transport.device(), &name, crate::sync::LOCAL_PLATFORM) {
                Ok(payload) if !payload.is_empty() => {
                    commits.write(LocalCommit {
                        doc: DocKey::WORKSPACE,
                        payload,
                    });
                }
                Ok(_) => {}
                Err(err) => tracing::error!(%err, "renaming device failed"),
            }
        }
        Some(crate::settings::SettingsAction::Unpair) => {
            // Our row goes first so the removal rides the connections we
            // are about to drop; the node tears them down after a grace.
            match docs.remove_device(transport.device()) {
                Ok(payload) if !payload.is_empty() => {
                    commits.write(LocalCommit {
                        doc: DocKey::WORKSPACE,
                        payload,
                    });
                }
                Ok(_) => {}
                Err(err) => tracing::error!(%err, "removing own device row failed"),
            }
            if let Err(err) = crate::config::persist_unpair() {
                tracing::error!(%err, "persisting unpair failed");
            }
            let own = settings.unpaired_info();
            let node = sync.node.clone();
            let old_token = std::mem::replace(&mut settings.info.token, own.token.clone());
            if old_token != own.token {
                node.remove_token(&old_token);
            }
            runtime.0.spawn(async move {
                tokio::time::sleep(crate::settings::UNPAIR_LINGER).await;
                node.set_peers(Vec::new()).await;
                if let Err(err) = node.set_relay(None).await {
                    tracing::warn!(%err, "dropping relay after leaving failed");
                }
            });
            settings.adopt(own);
        }
        Some(crate::settings::SettingsAction::Theme(flavor)) => {
            theme.set_flavor(flavor);
            if let Err(err) = crate::config::persist_theme(flavor) {
                tracing::error!(%err, "persisting theme failed");
            }
        }
        None => {}
    }

    if follow.0
        && let Some(newest) = docs.workspace.notes().first().map(|n| n.id)
        && editor.open != Some(newest)
    {
        match docs.open_note(newest) {
            Ok(_) => {
                subscribes.write(SubscribeNeeded(DocKey::from(newest)));
                open_note(&mut docs, &mut editor, newest);
            }
            Err(err) => tracing::error!(%err, "follow-latest open failed"),
        }
    }
    // egui 0.36 panels render into a Ui; build the screen-covering root.
    let mut root = egui::Ui::new(
        ctx.clone(),
        "root".into(),
        egui::UiBuilder::new()
            .layer_id(egui::LayerId::background())
            .max_rect(ctx.viewport_rect()),
    );

    // Fold in remote changes before the widget reads the buffer.
    if editor.remote_dirty {
        editor.remote_dirty = false;
        if let Some(id) = editor.open
            && let Some(note) = docs.note(id)
        {
            let text = note.text();
            if text != editor.last {
                editor.buffer = text.clone();
                editor.last = text;
            }
        }
    }

    egui::Panel::left("library")
        .default_size(250.0)
        .min_size(200.0)
        .frame(
            egui::Frame::new()
                .fill(palette.sidebar)
                .inner_margin(egui::Margin::symmetric(12, 16)),
        )
        .show(&mut root, |ui| {
            brand(ui, &palette);
            ui.add_space(14.0);
            if ui
                .add_sized(
                    [ui.available_width(), 34.0],
                    palette.primary_button("+  New note"),
                )
                .clicked()
            {
                create_note(&mut docs, &mut editor, &mut commits, &mut subscribes);
            }
            ui.add_space(18.0);

            let notes = docs.workspace.notes();
            ui.horizontal(|ui| {
                palette.caption(ui, "Notes");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(notes.len().to_string())
                            .small()
                            .color(palette.muted),
                    );
                });
            });
            ui.add_space(4.0);

            // Leave room for the footer so the list scrolls above it.
            let footer_height = 44.0;
            let list_height = (ui.available_height() - footer_height).max(0.0);
            egui::ScrollArea::vertical()
                .id_salt("notes")
                .max_height(list_height)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if notes.is_empty() {
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new("No notes yet. Create one above.")
                                .color(palette.muted),
                        );
                    }
                    for meta in notes {
                        let selected = editor.open == Some(meta.id);
                        if note_row(ui, &palette, &meta.title, selected).clicked() && !selected {
                            match docs.open_note(meta.id) {
                                Ok(_) => {
                                    subscribes.write(SubscribeNeeded(DocKey::from(meta.id)));
                                    open_note(&mut docs, &mut editor, meta.id);
                                }
                                Err(err) => tracing::error!(%err, "open note failed"),
                            }
                        }
                    }
                });

            ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                let label = egui::RichText::new("⚙  Settings").color(palette.muted);
                let button = egui::Button::new(label)
                    .fill(egui::Color32::TRANSPARENT)
                    .stroke(egui::Stroke::NONE)
                    .min_size(egui::Vec2::new(ui.available_width(), 32.0));
                if ui.add(button).clicked() {
                    settings.open = !settings.open;
                }
            });
        });

    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(palette.bg)
                .inner_margin(egui::Margin::same(18)),
        )
        .show(&mut root, |ui| {
            let Some(id) = editor.open else {
                empty_state(
                    ui,
                    &palette,
                    &mut docs,
                    &mut editor,
                    &mut commits,
                    &mut subscribes,
                );
                return;
            };

            let title = docs
                .workspace
                .notes()
                .iter()
                .find(|n| n.id == id)
                .map(|n| n.title.clone())
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(|| "Untitled".to_owned());
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(title).heading().color(palette.text));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let remote: Vec<_> = view
                        .peers
                        .iter()
                        .filter(|p| p.kind != krabink_local::PeerKind::Local)
                        .collect();
                    let all_up = remote
                        .iter()
                        .all(|p| matches!(p.state, krabink_local::PeerState::Connected { .. }));
                    let (color, label) = if remote.is_empty() {
                        (palette.muted, "no peers")
                    } else if all_up {
                        (palette.success, "live sync")
                    } else {
                        (palette.warn, "sync connecting…")
                    };
                    palette.status_dot(ui, color, label);
                    ui.add_space(8.0);
                    palette.toggle_switch(ui, &mut editor.preview, "Preview");
                });
            });
            ui.add_space(12.0);

            let editor_height = ui.available_height();
            let preview = editor.preview;
            let (output, inner_rect) = pane(ui, &palette, editor_height, |ui| {
                let scroll = egui::ScrollArea::vertical()
                    .id_salt("editor")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // Reserved before the text so the page ink lands
                        // under it; filled in once the galley position is
                        // known.
                        let under = ui.painter().add(egui::Shape::Noop);
                        let available = ui.available_size();
                        let EditorState {
                            buffer,
                            styled,
                            previewed,
                            ..
                        } = &mut *editor;
                        // The reading view shows the display text (read-only)
                        // with the runs remapped onto it; the editor the
                        // source with its own runs.
                        let (text, runs): (&mut String, &[StyleRun]) = if preview {
                            previewed.refresh(buffer);
                            (&mut previewed.display, &previewed.runs)
                        } else {
                            let runs = styled.runs_for(buffer);
                            (buffer, runs)
                        };
                        let mut layouter =
                            |ui: &egui::Ui, text: &dyn egui::TextBuffer, wrap_width: f32| {
                                let job = layout_job(text.as_str(), runs, &palette, wrap_width);
                                ui.painter().layout_job(job)
                            };
                        let output = egui::TextEdit::multiline(text)
                            .layouter(&mut layouter)
                            .interactive(!preview)
                            .frame(egui::Frame::NONE)
                            .desired_width(f32::INFINITY)
                            .min_size(available)
                            .hint_text("Start writing…")
                            .show(ui);
                        ui.add_space(available.y * TAIL_FRACTION);
                        if let Some(target) = &page_texture.0 {
                            // Painted in galley space, so a one-frame-old
                            // window still lands on the right text.
                            let rect = egui::Rect::from_min_size(
                                output.galley_pos + target.window_min,
                                target.size,
                            );
                            ui.painter().set(
                                under,
                                egui::Shape::image(
                                    target.texture,
                                    rect,
                                    egui::Rect::from_min_max(
                                        egui::Pos2::ZERO,
                                        egui::pos2(1.0, 1.0),
                                    ),
                                    egui::Color32::WHITE,
                                ),
                            );
                        }
                        output
                    });
                (scroll.inner, scroll.inner_rect)
            });

            if !preview && output.response.response.changed() {
                let (buffer, last) = (editor.buffer.clone(), editor.last.clone());
                if let Some((at, del, insert)) = splice_of(&last, &buffer) {
                    match docs.splice(id, at, del, &insert) {
                        Ok(payload) => {
                            editor.last = buffer;
                            commits.write(LocalCommit {
                                doc: DocKey::from(id),
                                payload,
                            });
                            match docs.refresh_meta(id) {
                                Ok(meta) => {
                                    if let Some(payload) = meta.note {
                                        commits.write(LocalCommit {
                                            doc: DocKey::from(id),
                                            payload,
                                        });
                                    }
                                    if let Some(payload) = meta.workspace {
                                        commits.write(LocalCommit {
                                            doc: DocKey::WORKSPACE,
                                            payload,
                                        });
                                    }
                                }
                                Err(err) => tracing::error!(%err, "meta refresh failed"),
                            }
                        }
                        Err(err) => tracing::error!(%err, "splice failed"),
                    }
                }
            }

            if let Some(note) = docs.note(id) {
                let window = inner_rect.translate(-output.galley_pos.to_vec2());
                let map = preview.then(|| editor.previewed.source_of.clone());
                layout.update(id, note, output.galley, window, scale, map);
            }
        });

    Ok(())
}

/// Logo mark + app name at the top of the sidebar.
fn brand(ui: &mut egui::Ui, palette: &Palette) {
    ui.horizontal(|ui| {
        logo(ui, palette, 26.0, palette.sidebar);
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("krabink")
                .size(21.0)
                .strong()
                .color(palette.text),
        );
    });
}

/// The ring logo mark, `size` wide, with `bg` as the ring's inner colour.
fn logo(ui: &mut egui::Ui, palette: &Palette, size: f32, bg: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat(size), egui::Sense::hover());
    let painter = ui.painter();
    let c = rect.center();
    painter.circle_filled(c, size * 0.46, palette.accent);
    painter.circle_filled(c, size * 0.25, bg);
    painter.circle_filled(c, size * 0.1, palette.accent);
}

/// One entry in the library list: rounded hover/selection background with
/// an accent bar on the selected row, title truncated to one line.
fn note_row(ui: &mut egui::Ui, palette: &Palette, title: &str, selected: bool) -> egui::Response {
    let height = 34.0;
    let (rect, response) = ui.allocate_exact_size(
        egui::Vec2::new(ui.available_width(), height),
        egui::Sense::click(),
    );
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let fill = if selected {
        palette.accent_soft()
    } else if response.hovered() {
        palette.surface_raised
    } else {
        egui::Color32::TRANSPARENT
    };
    let painter = ui.painter();
    painter.rect_filled(rect, egui::CornerRadius::same(theme::RADIUS), fill);
    if selected {
        let bar = egui::Rect::from_min_size(
            rect.left_top() + egui::Vec2::new(0.0, 9.0),
            egui::Vec2::new(3.0, height - 18.0),
        );
        painter.rect_filled(bar, egui::CornerRadius::same(2), palette.accent);
    }
    let shown = if title.trim().is_empty() {
        "Untitled"
    } else {
        title
    };
    let color = if selected || response.hovered() {
        palette.text
    } else {
        palette.muted
    };
    let text_width = rect.width() - 24.0;
    let galley = egui::WidgetText::from(egui::RichText::new(shown).color(color)).into_galley(
        ui,
        Some(egui::TextWrapMode::Truncate),
        text_width,
        egui::TextStyle::Body,
    );
    let pos = egui::pos2(rect.left() + 12.0, rect.center().y - galley.size().y / 2.0);
    painter.galley(pos, galley, color);
    response
}

/// Paper card filling `height`: the editor's frame.
fn pane<R>(
    ui: &mut egui::Ui,
    palette: &Palette,
    height: f32,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    palette
        .card()
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_min_height(height - 26.0);
            ui.set_width(ui.available_width());
            body(ui)
        })
        .inner
}

/// What the central area shows when no note is open.
fn empty_state(
    ui: &mut egui::Ui,
    palette: &Palette,
    docs: &mut Docs,
    editor: &mut EditorState,
    commits: &mut MessageWriter<LocalCommit>,
    subscribes: &mut MessageWriter<SubscribeNeeded>,
) {
    let rect = ui.available_rect_before_wrap();
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
        ui.add_space((rect.height() / 2.0 - 90.0).max(0.0));
        ui.vertical_centered(|ui| {
            logo(ui, palette, 48.0, palette.bg);
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("No note open")
                    .size(22.0)
                    .strong()
                    .color(palette.text),
            );
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("Pick a note from the library, or start a fresh one.")
                    .color(palette.muted),
            );
            ui.add_space(16.0);
            if ui
                .add_sized([160.0, 34.0], palette.primary_button("Create a note"))
                .clicked()
            {
                create_note(docs, editor, commits, subscribes);
            }
        });
    });
}

/// Create a note, broadcast it, and open it in the editor.
fn create_note(
    docs: &mut Docs,
    editor: &mut EditorState,
    commits: &mut MessageWriter<LocalCommit>,
    subscribes: &mut MessageWriter<SubscribeNeeded>,
) {
    match docs.create_note() {
        Ok((id, note_payload, ws_payload)) => {
            commits.write(LocalCommit {
                doc: DocKey::from(id),
                payload: note_payload,
            });
            commits.write(LocalCommit {
                doc: DocKey::WORKSPACE,
                payload: ws_payload,
            });
            subscribes.write(SubscribeNeeded(DocKey::from(id)));
            open_note(docs, editor, id);
        }
        Err(err) => tracing::error!(%err, "create note failed"),
    }
}

fn open_note(docs: &mut Docs, editor: &mut EditorState, id: NoteId) {
    let text = docs.note(id).map(|n| n.text()).unwrap_or_default();
    editor.open = Some(id);
    editor.buffer = text.clone();
    editor.last = text;
}

// ---- styled source ----

/// How one character renders; runs paint over a per-char array of these,
/// outer runs first, so inner runs win.
#[derive(Debug, Clone, Copy, PartialEq)]
struct CharStyle {
    size: f32,
    mono: bool,
    italics: bool,
    underline: bool,
    strikethrough: bool,
    color: Role,
    background: bool,
    /// Left indent of the line this char starts, if it starts one.
    indent: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Body,
    Strong,
    Muted,
    Accent,
}

impl CharStyle {
    const BODY: Self = Self {
        size: BODY_SIZE,
        mono: false,
        italics: false,
        underline: false,
        strikethrough: false,
        color: Role::Body,
        background: false,
        indent: 0.0,
    };

    fn apply(&mut self, kind: StyleKind) {
        match kind {
            StyleKind::Heading { level } => {
                let scale = HEADING_SCALE[usize::from(level.clamp(1, 6)) - 1];
                self.size = BODY_SIZE * scale;
                self.color = Role::Strong;
            }
            StyleKind::Strong => self.color = Role::Strong,
            StyleKind::Emphasis => self.italics = true,
            StyleKind::Strikethrough => self.strikethrough = true,
            StyleKind::CodeSpan | StyleKind::CodeBlock => {
                self.mono = true;
                self.background = true;
            }
            StyleKind::ListItem { depth, .. } => self.indent = f32::from(depth) * LIST_INDENT,
            StyleKind::BlockQuote => {
                self.italics = true;
                self.indent += LIST_INDENT;
            }
            StyleKind::Link => {
                self.color = Role::Accent;
                self.underline = true;
            }
            StyleKind::Marker | StyleKind::ThematicBreak => {
                self.color = Role::Muted;
                self.underline = false;
            }
        }
    }

    fn format(&self, palette: &Palette) -> TextFormat {
        let family = if self.mono {
            egui::FontFamily::Monospace
        } else {
            egui::FontFamily::Proportional
        };
        // No bundled bold face: strong text is full-strength ink, body
        // text slightly softened.
        let color = match self.color {
            Role::Body => palette.text.gamma_multiply(0.85),
            Role::Strong => palette.text,
            Role::Muted => palette.muted,
            Role::Accent => palette.accent,
        };
        let line = |on: bool| {
            if on {
                egui::Stroke::new(1.0, color)
            } else {
                egui::Stroke::NONE
            }
        };
        TextFormat {
            font_id: egui::FontId::new(self.size, family),
            color,
            background: if self.background {
                palette.surface_raised
            } else {
                egui::Color32::TRANSPARENT
            },
            italics: self.italics,
            underline: line(self.underline),
            strikethrough: line(self.strikethrough),
            ..TextFormat::default()
        }
    }
}

/// Per-char styles of `text` under `runs` (scalar offsets, clamped).
fn char_styles(text: &str, runs: &[StyleRun]) -> Vec<CharStyle> {
    let mut styles = vec![CharStyle::BODY; text.chars().count()];
    for run in runs {
        let end = run.end.min(styles.len());
        for style in &mut styles[run.start.min(end)..end] {
            style.apply(run.kind);
        }
    }
    styles
}

/// The styled galley job: one section per maximal run of equally styled
/// chars, split at newlines so every line of an indented block indents.
fn layout_job(text: &str, runs: &[StyleRun], palette: &Palette, wrap_width: f32) -> LayoutJob {
    let styles = char_styles(text, runs);
    let mut job = LayoutJob {
        text: text.to_owned(),
        ..LayoutJob::default()
    };
    job.wrap.max_width = wrap_width;

    let mut section_start = 0usize; // byte
    let mut section_style: Option<CharStyle> = None;
    let mut line_start = true;
    let push =
        |job: &mut LayoutJob, start: usize, end: usize, style: CharStyle, at_line_start: bool| {
            job.sections.push(LayoutSection {
                leading_space: if at_line_start { style.indent } else { 0.0 },
                byte_range: ByteIndex(start)..ByteIndex(end),
                format: style.format(palette),
            });
        };
    let mut section_at_line_start = true;
    for ((byte, ch), style) in text.char_indices().zip(&styles) {
        let split = section_style.is_some_and(|s| s != *style) || (line_start && byte > 0);
        if split {
            if let Some(s) = section_style {
                push(&mut job, section_start, byte, s, section_at_line_start);
            }
            section_start = byte;
            section_at_line_start = line_start;
        }
        section_style = Some(*style);
        line_start = ch == '\n';
    }
    if let Some(s) = section_style {
        push(
            &mut job,
            section_start,
            text.len(),
            s,
            section_at_line_start,
        );
    }
    if job.sections.is_empty() {
        push(&mut job, 0, 0, CharStyle::BODY, true);
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> Palette {
        *Theme::new(theme::Flavor::Latte).palette()
    }

    fn sections(text: &str) -> Vec<(std::ops::Range<usize>, f32, f32)> {
        let job = layout_job(text, &style_runs(text), &palette(), 400.0);
        job.sections
            .iter()
            .map(|s| {
                (
                    s.byte_range.start.0..s.byte_range.end.0,
                    s.format.font_id.size,
                    s.leading_space,
                )
            })
            .collect()
    }

    #[test]
    fn splice_detects_middle_change() {
        assert_eq!(splice_of("abc", "aXc"), Some((1, 1, "X".into())));
        assert_eq!(splice_of("abc", "abc"), None);
    }

    #[test]
    fn heading_scales_and_marker_dims() {
        let text = "# Hi\nbody";
        let job = layout_job(text, &style_runs(text), &palette(), 400.0);
        let marker = &job.sections[0];
        assert_eq!(marker.byte_range, ByteIndex(0)..ByteIndex(1));
        assert_eq!(marker.format.color, palette().muted);
        assert_eq!(marker.format.font_id.size, BODY_SIZE * HEADING_SCALE[0]);
        let body = job.sections.last().unwrap();
        assert_eq!(body.byte_range, ByteIndex(5)..ByteIndex(9));
        assert_eq!(body.format.font_id.size, BODY_SIZE);
    }

    #[test]
    fn every_list_line_indents() {
        let text = "- a\n- b\n";
        let got = sections(text);
        let line_starts: Vec<_> = got
            .iter()
            .filter(|(r, _, _)| r.start == 0 || r.start == 4)
            .collect();
        assert_eq!(line_starts.len(), 2);
        assert!(line_starts.iter().all(|(_, _, lead)| *lead == LIST_INDENT));
        // The marker's section is the only one carrying the indent.
        assert!(got.iter().filter(|(_, _, lead)| *lead > 0.0).count() == 2);
    }

    #[test]
    fn sections_cover_text_in_order_with_multibyte() {
        let text = "héllo **wörld** `c`\n> q\n";
        let job = layout_job(text, &style_runs(text), &palette(), 400.0);
        let mut at = 0;
        for s in &job.sections {
            assert_eq!(s.byte_range.start.0, at);
            assert!(text.is_char_boundary(s.byte_range.end.0));
            at = s.byte_range.end.0;
        }
        assert_eq!(at, text.len());
        // Backticks are markers inside the span: three sections, all mono.
        let mono: String = job
            .sections
            .iter()
            .filter(|s| s.format.font_id.family == egui::FontFamily::Monospace)
            .map(|s| &text[s.byte_range.start.0..s.byte_range.end.0])
            .collect();
        assert_eq!(mono, "`c`");
    }

    #[test]
    fn empty_text_has_one_section() {
        assert_eq!(sections("").len(), 1);
    }

    #[test]
    fn out_of_range_runs_are_clamped() {
        let runs = [StyleRun {
            start: 2,
            end: 99,
            kind: StyleKind::Strong,
        }];
        let styles = char_styles("abc", &runs);
        assert_eq!(styles.len(), 3);
        assert_eq!(styles[2].color, Role::Strong);
    }
}
