//! The desktop app's document state: workspace registry + open notes over the
//! shared redb store. Every local or remote change is persisted here; the
//! sync layer only moves opaque payloads.

use std::collections::HashMap;

use bevy::prelude::Resource;
use pendant_core::{
    ClientDocs, DeviceId, DocKey, Flush, NoteDoc, NoteId, NoteMeta, Store, WorkspaceDoc,
};

/// Compact a doc when its stored update log grows past this.
const COMPACT_AFTER_UPDATES: u64 = 500;

/// Sync payloads produced by [`Docs::refresh_meta`], one per doc touched.
#[derive(Default)]
pub struct MetaRefresh {
    pub note: Option<Vec<u8>>,
    pub workspace: Option<Vec<u8>>,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Resource)]
pub struct Docs {
    store: Store,
    pub workspace: WorkspaceDoc,
    open: HashMap<NoteId, NoteDoc>,
}

impl Docs {
    pub fn load(store: Store) -> pendant_core::Result<Self> {
        let stored = store.load(DocKey::WORKSPACE)?;
        let workspace = WorkspaceDoc::from_bytes(
            stored.snapshot.as_deref(),
            stored.updates.iter().map(Vec::as_slice),
        )?;
        Ok(Self {
            store,
            workspace,
            open: HashMap::new(),
        })
    }

    pub fn note(&self, id: NoteId) -> Option<&NoteDoc> {
        self.open.get(&id)
    }

    /// Open (loading from the store if needed) and compact when overdue.
    pub fn open_note(&mut self, id: NoteId) -> pendant_core::Result<&NoteDoc> {
        let key = DocKey::from(id);
        if !self.open.contains_key(&id) {
            let stored = self.store.load(key)?;
            let note = NoteDoc::from_bytes(
                id,
                stored.snapshot.as_deref(),
                stored.updates.iter().map(Vec::as_slice),
            )?;
            if self.store.update_count(key)? > COMPACT_AFTER_UPDATES {
                self.store.checkpoint(key, &note.export_snapshot()?)?;
            }
            self.open.insert(id, note);
        }
        Ok(self.open.get(&id).expect("inserted above"))
    }

    /// Create a note, register it in the workspace. Returns the id plus the
    /// two payloads (note, workspace) that need syncing.
    pub fn create_note(&mut self) -> pendant_core::Result<(NoteId, Vec<u8>, Vec<u8>)> {
        let id = NoteId::new();
        let note = NoteDoc::new(id);
        note.set_title("untitled")?;
        let note_payload = note.export_updates_since(&[])?;
        self.persist(DocKey::from(id), &note_payload)?;
        self.open.insert(id, note);

        let ws_before = self.workspace.version();
        self.workspace.upsert(&NoteMeta {
            id,
            title: "untitled".into(),
            archived: false,
            updated_ms: now_ms(),
        })?;
        let ws_payload = self.workspace.export_updates_since(&ws_before)?;
        self.persist(DocKey::WORKSPACE, &ws_payload)?;

        Ok((id, note_payload, ws_payload))
    }

    /// Splice an edit into an open note; returns the sync payload.
    pub fn splice(
        &mut self,
        id: NoteId,
        at: usize,
        del: usize,
        insert: &str,
    ) -> pendant_core::Result<Vec<u8>> {
        let note = self.open.get(&id).expect("splice on unopened note");
        let before = note.version();
        note.splice_text(at, del, insert)?;
        let payload = note.export_updates_since(&before)?;
        self.persist(DocKey::from(id), &payload)?;
        Ok(payload)
    }

    /// Upsert this device into the synced registry (called on every
    /// connect); returns the workspace payload for the wire.
    pub fn register_device(
        &mut self,
        id: DeviceId,
        name: &str,
        platform: &str,
    ) -> pendant_core::Result<Vec<u8>> {
        let before = self.workspace.version();
        self.workspace.upsert_device(&pendant_core::DeviceMeta {
            id,
            name: name.into(),
            platform: platform.into(),
            last_seen_ms: now_ms(),
        })?;
        let payload = self.workspace.export_updates_since(&before)?;
        self.persist(DocKey::WORKSPACE, &payload)?;
        Ok(payload)
    }

    /// Drop `id` from the synced device registry; returns the workspace
    /// payload for the wire.
    pub fn remove_device(&mut self, id: DeviceId) -> pendant_core::Result<Vec<u8>> {
        let before = self.workspace.version();
        self.workspace.remove_device(id)?;
        let payload = self.workspace.export_updates_since(&before)?;
        self.persist(DocKey::WORKSPACE, &payload)?;
        Ok(payload)
    }

    /// Keep the note's own title and the workspace registry's title/updated
    /// fresh for `id`. Returns the payloads that changed, one per doc.
    pub fn refresh_meta(&mut self, id: NoteId) -> pendant_core::Result<MetaRefresh> {
        let mut out = MetaRefresh::default();
        let Some(note) = self.open.get(&id) else {
            return Ok(out);
        };
        let title = note
            .text()
            .lines()
            .next()
            .unwrap_or("")
            .trim_start_matches(['#', ' '])
            .chars()
            .take(60)
            .collect::<String>();
        let title = if title.is_empty() {
            "untitled".into()
        } else {
            title
        };
        // The title is an op on the note doc. It must be exported and
        // persisted like any edit: every later text op depends on it, so a
        // peer that never received it can apply nothing that follows.
        if note.title().as_deref() != Some(title.as_str()) {
            let before = note.version();
            note.set_title(&title)?;
            let payload = note.export_updates_since(&before)?;
            self.persist(DocKey::from(id), &payload)?;
            out.note = Some(payload);
        }

        let current = self.workspace.notes().into_iter().find(|n| n.id == id);
        if current.as_ref().is_some_and(|n| n.title == title) {
            return Ok(out);
        }
        let before = self.workspace.version();
        self.workspace.upsert(&NoteMeta {
            id,
            title,
            archived: false,
            updated_ms: now_ms(),
        })?;
        let payload = self.workspace.export_updates_since(&before)?;
        self.persist(DocKey::WORKSPACE, &payload)?;
        out.workspace = Some(payload);
        Ok(out)
    }

    pub fn version_of(&self, key: DocKey) -> Vec<u8> {
        if key == DocKey::WORKSPACE {
            self.workspace.version()
        } else {
            self.open
                .iter()
                .find(|(id, _)| DocKey::from(**id) == key)
                .map(|(_, note)| note.version())
                .unwrap_or_default()
        }
    }

    fn persist(&self, key: DocKey, payload: &[u8]) -> pendant_core::Result<()> {
        // Immediate keeps the loss window at zero; typing-rate fsyncs are
        // fine on desktop hardware. Revisit with batching if it ever shows.
        self.store.append_update(key, payload, Flush::Immediate)
    }
}

impl ClientDocs for Docs {
    fn import(&mut self, doc: DocKey, payload: &[u8]) -> pendant_core::Result<()> {
        if payload.is_empty() {
            return Ok(());
        }
        if doc == DocKey::WORKSPACE {
            self.workspace.import_update(payload)?;
        } else if let Some((_, note)) = self.open.iter().find(|(id, _)| DocKey::from(**id) == doc) {
            note.import_update(payload)?;
        } else {
            return Ok(()); // not open; server keeps it, nothing to do
        }
        self.persist(doc, payload)
    }

    fn updates_since(&mut self, doc: DocKey, have: &[u8]) -> pendant_core::Result<Vec<u8>> {
        if doc == DocKey::WORKSPACE {
            self.workspace.export_updates_since(have)
        } else {
            self.open
                .iter()
                .find(|(id, _)| DocKey::from(**id) == doc)
                .map(|(_, note)| note.export_updates_since(have))
                .unwrap_or_else(|| Ok(Vec::new()))
        }
    }
}
