//! Editor UI: library sidebar, markdown text editor bound to the CRDT via
//! prefix/suffix diffing, and a live preview pane.

use bevy::prelude::*;
use bevy_egui::{EguiContexts, EguiPrimaryContextPass, egui};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use pendant_core::{DocKey, NoteId};

use crate::docs::Docs;
use crate::pairing::PairShare;
use crate::sync::{LocalCommit, SubscribeNeeded};

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
            .add_systems(EguiPrimaryContextPass, editor_ui);
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
    mut pair: ResMut<PairShare>,
    follow: Res<FollowLatest>,
) -> Result {
    let ctx = contexts.ctx_mut()?;
    pair.window(ctx);

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
        .default_size(220.0)
        .show(&mut root, |ui| {
            ui.heading("pendant");
            ui.separator();
            if ui.button("➕ new note").clicked() {
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
                        open_note(&mut docs, &mut editor, id);
                    }
                    Err(err) => tracing::error!(%err, "create note failed"),
                }
            }
            if pair.info.is_some() && ui.button("⧉ pair device").clicked() {
                pair.open = !pair.open;
            }
            ui.separator();
            for meta in docs.workspace.notes() {
                let selected = editor.open == Some(meta.id);
                if ui.selectable_label(selected, &meta.title).clicked() && !selected {
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

    egui::CentralPanel::default().show(&mut root, |ui| {
        let Some(id) = editor.open else {
            ui.centered_and_justified(|ui| ui.label("select or create a note"));
            return;
        };

        ui.columns(2, |columns| {
            let response = egui::ScrollArea::vertical()
                .id_salt("editor")
                .show(&mut columns[0], |ui| {
                    ui.add_sized(
                        ui.available_size(),
                        egui::TextEdit::multiline(&mut editor.buffer).code_editor(),
                    )
                })
                .inner;

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

            egui::ScrollArea::vertical()
                .id_salt("preview")
                .show(&mut columns[1], |ui| {
                    CommonMarkViewer::new().show(ui, &mut markdown.0, &editor.buffer);
                });
        });
    });

    Ok(())
}

fn open_note(docs: &mut Docs, editor: &mut EditorState, id: NoteId) {
    let text = docs.note(id).map(|n| n.text()).unwrap_or_default();
    editor.open = Some(id);
    editor.buffer = text.clone();
    editor.last = text;
}
