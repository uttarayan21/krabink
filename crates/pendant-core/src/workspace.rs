//! The workspace document: a small CRDT registry of all notes, used to drive
//! library UI (note list, titles) without opening every note doc.

use loro::{ExportMode, LoroDoc, LoroMap, LoroValue, ValueOrContainer};
use serde::{Deserialize, Serialize};

use crate::{NoteId, Result};

/// Entry in the note registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteMeta {
    pub id: NoteId,
    pub title: String,
    pub archived: bool,
    /// Unix millis of the last edit this device knows about.
    pub updated_ms: u64,
}

/// CRDT doc listing every note in the library.
pub struct WorkspaceDoc {
    doc: LoroDoc,
}

const NOTES: &str = "notes";

impl WorkspaceDoc {
    pub fn new() -> Self {
        Self {
            doc: LoroDoc::new(),
        }
    }

    pub fn from_bytes<'a>(
        snapshot: Option<&[u8]>,
        updates: impl IntoIterator<Item = &'a [u8]>,
    ) -> Result<Self> {
        let ws = Self::new();
        if let Some(snapshot) = snapshot {
            ws.doc.import(snapshot)?;
        }
        for update in updates {
            ws.doc.import(update)?;
        }
        Ok(ws)
    }

    pub fn upsert(&self, meta: &NoteMeta) -> Result<()> {
        let notes = self.doc.get_map(NOTES);
        let entry = notes.insert_container(&meta.id.to_string(), LoroMap::new())?;
        entry.insert("title", meta.title.as_str())?;
        entry.insert("archived", meta.archived)?;
        entry.insert("updated", meta.updated_ms as i64)?;
        self.doc.commit();
        Ok(())
    }

    pub fn remove(&self, id: NoteId) -> Result<()> {
        self.doc.get_map(NOTES).delete(&id.to_string())?;
        self.doc.commit();
        Ok(())
    }

    /// All notes, newest edit first.
    pub fn notes(&self) -> Vec<NoteMeta> {
        let notes = self.doc.get_map(NOTES);
        let mut out: Vec<NoteMeta> = notes
            .keys()
            .filter_map(|key| {
                let id = key.parse().ok()?;
                let entry = match notes.get(&key)? {
                    ValueOrContainer::Container(c) => c.into_map().ok()?,
                    ValueOrContainer::Value(_) => return None,
                };
                let get = |k: &str| entry.get(k);
                Some(NoteMeta {
                    id,
                    title: match get("title")? {
                        ValueOrContainer::Value(LoroValue::String(s)) => s.to_string(),
                        _ => return None,
                    },
                    archived: matches!(
                        get("archived")?,
                        ValueOrContainer::Value(LoroValue::Bool(true))
                    ),
                    updated_ms: match get("updated")? {
                        ValueOrContainer::Value(LoroValue::I64(v)) => v as u64,
                        _ => 0,
                    },
                })
            })
            .collect();
        out.sort_by_key(|n| std::cmp::Reverse(n.updated_ms));
        out
    }

    // Same sync surface as NoteDoc.

    pub fn version(&self) -> Vec<u8> {
        self.doc.oplog_vv().encode()
    }

    pub fn export_updates_since(&self, since: &[u8]) -> Result<Vec<u8>> {
        let from = loro::VersionVector::decode(since).unwrap_or_default();
        Ok(self.doc.export(ExportMode::updates(&from))?)
    }

    pub fn export_snapshot(&self) -> Result<Vec<u8>> {
        Ok(self.doc.export(ExportMode::Snapshot)?)
    }

    pub fn import_update(&self, bytes: &[u8]) -> Result<()> {
        self.doc.import(bytes)?;
        Ok(())
    }
}

impl Default for WorkspaceDoc {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_roundtrip() {
        let a = WorkspaceDoc::new();
        let note = NoteMeta {
            id: NoteId::new(),
            title: "groceries".into(),
            archived: false,
            updated_ms: 42,
        };
        a.upsert(&note).unwrap();

        let b = WorkspaceDoc::new();
        b.import_update(&a.export_updates_since(&[]).unwrap())
            .unwrap();
        assert_eq!(b.notes(), vec![note.clone()]);

        b.remove(note.id).unwrap();
        a.import_update(&b.export_updates_since(&a.version()).unwrap())
            .unwrap();
        assert!(a.notes().is_empty());
    }
}
