//! Editor UI: library sidebar, markdown text editor bound to the CRDT via
//! prefix/suffix diffing, and a live preview pane.

use bevy::prelude::*;
use bevy_egui::{EguiContexts, EguiPrimaryContextPass, egui};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use pendant_core::{DocKey, NoteId};

use crate::docs::Docs;
use crate::settings::Settings;
use crate::sync::SyncTransport;
use crate::sync::{LocalCommit, SubscribeNeeded};
use crate::theme;

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
}

#[derive(Default)]
struct MarkdownCache(CommonMarkCache);

pub struct EditorUiPlugin;

impl Plugin for EditorUiPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<EditorState>()
            .init_resource::<FollowLatest>()
            .init_non_send::<MarkdownCache>()
            // Theme first so the very first frame already renders styled.
            .add_systems(EguiPrimaryContextPass, editor_ui.after(theme::install_once));
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
    mut markdown: NonSendMut<MarkdownCache>,
    mut commits: MessageWriter<LocalCommit>,
    mut subscribes: MessageWriter<SubscribeNeeded>,
    mut settings: ResMut<Settings>,
    transport: Res<SyncTransport>,
    relay: Res<crate::relay::EmbeddedRelay>,
    mut adopted: MessageWriter<crate::settings::PairAdopted>,
    follow: Res<FollowLatest>,
    paired: Option<Res<crate::discovery::PairedDesktop>>,
    finder: Res<crate::discovery::RelayFinder>,
) -> Result {
    let ctx = contexts.ctx_mut()?;
    let view = crate::settings::SettingsView {
        links: transport.links(),
        mdns_name: relay.mdns_name.clone(),
        paired_relay: paired.map(|p| p.relay_id.clone()),
        discovered: finder.last_found.clone(),
        this_device: transport.device(),
        devices: docs.workspace.devices(),
        now_ms: crate::docs::now_ms(),
    };
    match settings.window(ctx, &view) {
        Some(crate::settings::SettingsAction::Join(info)) => {
            adopted.write(crate::settings::PairAdopted(info));
        }
        Some(crate::settings::SettingsAction::RemoveDevice(id)) => match docs.remove_device(id) {
            Ok(payload) if !payload.is_empty() => {
                commits.write(LocalCommit {
                    doc: DocKey::WORKSPACE,
                    payload,
                });
            }
            Ok(_) => {}
            Err(err) => tracing::error!(%err, %id, "removing device failed"),
        },
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
                .fill(theme::SIDEBAR)
                .inner_margin(egui::Margin::symmetric(12, 16)),
        )
        .show(&mut root, |ui| {
            brand(ui);
            ui.add_space(14.0);
            if ui
                .add_sized(
                    [ui.available_width(), 34.0],
                    theme::primary_button("+  New note"),
                )
                .clicked()
            {
                create_note(&mut docs, &mut editor, &mut commits, &mut subscribes);
            }
            ui.add_space(18.0);

            let notes = docs.workspace.notes();
            ui.horizontal(|ui| {
                theme::caption(ui, "Notes");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(notes.len().to_string())
                            .small()
                            .color(theme::MUTED),
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
                                .color(theme::MUTED),
                        );
                    }
                    for meta in notes {
                        let selected = editor.open == Some(meta.id);
                        if note_row(ui, &meta.title, selected).clicked() && !selected {
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
                let label = egui::RichText::new("⚙  Settings").color(theme::MUTED);
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
                .fill(theme::BG)
                .inner_margin(egui::Margin::same(18)),
        )
        .show(&mut root, |ui| {
            let Some(id) = editor.open else {
                empty_state(ui, &mut docs, &mut editor, &mut commits, &mut subscribes);
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
                ui.label(egui::RichText::new(title).heading().color(theme::TEXT));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    theme::status_dot(ui, theme::SUCCESS, "live sync");
                });
            });
            ui.add_space(12.0);

            let pane_height = ui.available_height();
            ui.columns(2, |columns| {
                let response = pane(&mut columns[0], "Markdown", pane_height, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("editor")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.add_sized(
                                ui.available_size(),
                                egui::TextEdit::multiline(&mut editor.buffer)
                                    .code_editor()
                                    .frame(egui::Frame::NONE)
                                    .desired_width(f32::INFINITY)
                                    .hint_text("Start writing…"),
                            )
                        })
                        .inner
                });

                if response.changed() {
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
                                    Ok(Some(ws_payload)) => {
                                        commits.write(LocalCommit {
                                            doc: DocKey::WORKSPACE,
                                            payload: ws_payload,
                                        });
                                    }
                                    Ok(None) => {}
                                    Err(err) => tracing::error!(%err, "meta refresh failed"),
                                }
                            }
                            Err(err) => tracing::error!(%err, "splice failed"),
                        }
                    }
                }

                pane(&mut columns[1], "Preview", pane_height, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("preview")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            CommonMarkViewer::new().show(ui, &mut markdown.0, &editor.buffer);
                        });
                });
            });
        });

    Ok(())
}

/// Logo mark + app name at the top of the sidebar.
fn brand(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        logo(ui, 26.0, theme::SIDEBAR);
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("pendant")
                .size(21.0)
                .strong()
                .color(theme::TEXT),
        );
    });
}

/// The ring logo mark, `size` wide, with `bg` as the ring's inner colour.
fn logo(ui: &mut egui::Ui, size: f32, bg: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat(size), egui::Sense::hover());
    let painter = ui.painter();
    let c = rect.center();
    painter.circle_filled(c, size * 0.46, theme::ACCENT);
    painter.circle_filled(c, size * 0.25, bg);
    painter.circle_filled(c, size * 0.1, theme::ACCENT);
}

/// One entry in the library list: rounded hover/selection background with
/// an accent bar on the selected row, title truncated to one line.
fn note_row(ui: &mut egui::Ui, title: &str, selected: bool) -> egui::Response {
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
        theme::ACCENT_SOFT
    } else if response.hovered() {
        theme::SURFACE_RAISED
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
        painter.rect_filled(bar, egui::CornerRadius::same(2), theme::ACCENT);
    }
    let shown = if title.trim().is_empty() {
        "Untitled"
    } else {
        title
    };
    let color = if selected || response.hovered() {
        theme::TEXT
    } else {
        theme::MUTED
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

/// Titled card filling `height`, used for the editor and preview columns.
fn pane<R>(
    ui: &mut egui::Ui,
    title: &str,
    height: f32,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    theme::card()
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_min_height(height - 26.0);
            ui.set_width(ui.available_width());
            theme::caption(ui, title);
            ui.add_space(6.0);
            body(ui)
        })
        .inner
}

/// What the central area shows when no note is open.
fn empty_state(
    ui: &mut egui::Ui,
    docs: &mut Docs,
    editor: &mut EditorState,
    commits: &mut MessageWriter<LocalCommit>,
    subscribes: &mut MessageWriter<SubscribeNeeded>,
) {
    let rect = ui.available_rect_before_wrap();
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
        ui.add_space((rect.height() / 2.0 - 90.0).max(0.0));
        ui.vertical_centered(|ui| {
            logo(ui, 48.0, theme::BG);
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("No note open")
                    .size(22.0)
                    .strong()
                    .color(theme::TEXT),
            );
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("Pick a note from the library, or start a fresh one.")
                    .color(theme::MUTED),
            );
            ui.add_space(16.0);
            if ui
                .add_sized([160.0, 34.0], theme::primary_button("Create a note"))
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
