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

/// Entry in the device registry: one row per client that ever joined the
/// workspace. Rows are upserted on connect, so `last_seen_ms` is "last time
/// this device came online", not liveness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceMeta {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub last_seen_ms: u64,
}

/// CRDT doc listing every note in the library.
pub struct WorkspaceDoc {
    doc: LoroDoc,
}

const NOTES: &str = "notes";
const DEVICES: &str = "devices";

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

    pub fn upsert_device(&self, meta: &DeviceMeta) -> Result<()> {
        let devices = self.doc.get_map(DEVICES);
        let entry = devices.insert_container(&meta.id, LoroMap::new())?;
        entry.insert("name", meta.name.as_str())?;
        entry.insert("platform", meta.platform.as_str())?;
        entry.insert("seen", meta.last_seen_ms as i64)?;
        self.doc.commit();
        Ok(())
    }

    /// Forget a device: every peer's list loses the row. The device
    /// re-registers itself if it connects again with a valid token; this
    /// is housekeeping, not revocation (that needs a token rotation).
    pub fn remove_device(&self, id: &str) -> Result<()> {
        self.doc.get_map(DEVICES).delete(id)?;
        self.doc.commit();
        Ok(())
    }

    /// All devices that ever joined, most recently seen first.
    pub fn devices(&self) -> Vec<DeviceMeta> {
        let devices = self.doc.get_map(DEVICES);
        let mut out: Vec<DeviceMeta> = devices
            .keys()
            .filter_map(|key| {
                let entry = match devices.get(&key)? {
                    ValueOrContainer::Container(c) => c.into_map().ok()?,
                    ValueOrContainer::Value(_) => return None,
                };
                let get = |k: &str| entry.get(k);
                let string = |v: ValueOrContainer| match v {
                    ValueOrContainer::Value(LoroValue::String(s)) => Some(s.to_string()),
                    _ => None,
                };
                Some(DeviceMeta {
                    id: key.to_string(),
                    name: string(get("name")?)?,
                    platform: string(get("platform")?)?,
                    last_seen_ms: match get("seen")? {
                        ValueOrContainer::Value(LoroValue::I64(v)) => v as u64,
                        _ => 0,
                    },
                })
            })
            .collect();
        out.sort_by_key(|d| std::cmp::Reverse(d.last_seen_ms));
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

    #[test]
    fn device_registry_roundtrip() {
        let a = WorkspaceDoc::new();
        let ipad = DeviceMeta {
            id: "dev-a".into(),
            name: "iPad".into(),
            platform: "iPad16,3".into(),
            last_seen_ms: 10,
        };
        a.upsert_device(&ipad).unwrap();

        let b = WorkspaceDoc::new();
        b.import_update(&a.export_updates_since(&[]).unwrap())
            .unwrap();
        let desk = DeviceMeta {
            id: "dev-b".into(),
            name: "desk".into(),
            platform: "linux".into(),
            last_seen_ms: 20,
        };
        b.upsert_device(&desk).unwrap();
        a.import_update(&b.export_updates_since(&a.version()).unwrap())
            .unwrap();

        // Most recently seen first; re-upsert bumps, not duplicates.
        assert_eq!(a.devices(), vec![desk.clone(), ipad.clone()]);
        a.upsert_device(&DeviceMeta {
            last_seen_ms: 30,
            ..ipad.clone()
        })
        .unwrap();
        assert_eq!(a.devices().len(), 2);
        assert_eq!(a.devices()[0].id, "dev-a");

        // Removal syncs; removing an unknown id is a no-op, not an error.
        b.remove_device("dev-a").unwrap();
        b.remove_device("nope").unwrap();
        a.import_update(&b.export_updates_since(&a.version()).unwrap())
            .unwrap();
        assert_eq!(a.devices().len(), 1);
        assert_eq!(a.devices()[0].id, "dev-b");
    }
}
