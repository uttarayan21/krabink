//! The workspace document: a small CRDT registry of all notes, used to drive
//! library UI (note list, titles) without opening every note doc.

use loro::{ExportMode, LoroDoc, LoroMap, LoroValue, ValueOrContainer};
use serde::{Deserialize, Serialize};

use crate::brush::BrushId;
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
    pub id: crate::DeviceId,
    pub name: String,
    pub platform: String,
    pub last_seen_ms: u64,
}

/// Entry in the brush library: a custom brush every device in the
/// workspace can pick. Strokes snapshot the spec inline, so editing or
/// deleting a library brush never restyles existing ink.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrushMeta {
    pub id: BrushId,
    pub name: String,
    /// Encoded [`crate::BrushSpec`] ([`crate::BrushSpec::encode`]); kept
    /// as bytes so a workspace with brushes from a newer build still
    /// lists them.
    pub spec: Vec<u8>,
    /// Unix millis of the last edit.
    pub updated_ms: u64,
}

/// CRDT doc listing every note in the library.
pub struct WorkspaceDoc {
    doc: LoroDoc,
}

const NOTES: &str = "notes";
const DEVICES: &str = "devices";
const BRUSHES: &str = "brushes";

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
        let entry = devices.insert_container(&meta.id.to_string(), LoroMap::new())?;
        entry.insert("name", meta.name.as_str())?;
        entry.insert("platform", meta.platform.as_str())?;
        entry.insert("seen", meta.last_seen_ms as i64)?;
        self.doc.commit();
        Ok(())
    }

    /// Forget a device: every peer's list loses the row. The device
    /// re-registers itself if it connects again with a valid token; this
    /// is housekeeping, not revocation (that needs a token rotation).
    pub fn remove_device(&self, id: crate::DeviceId) -> Result<()> {
        self.doc.get_map(DEVICES).delete(&id.to_string())?;
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
                    id: key.parse().ok()?,
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

    pub fn upsert_brush(&self, meta: &BrushMeta) -> Result<()> {
        let brushes = self.doc.get_map(BRUSHES);
        let entry = brushes.insert_container(&meta.id.0, LoroMap::new())?;
        entry.insert("name", meta.name.as_str())?;
        entry.insert("spec", LoroValue::Binary(meta.spec.clone().into()))?;
        entry.insert("updated", meta.updated_ms as i64)?;
        self.doc.commit();
        Ok(())
    }

    pub fn remove_brush(&self, id: &BrushId) -> Result<()> {
        self.doc.get_map(BRUSHES).delete(&id.0)?;
        self.doc.commit();
        Ok(())
    }

    /// Every library brush, newest edit first.
    pub fn brushes(&self) -> Vec<BrushMeta> {
        let brushes = self.doc.get_map(BRUSHES);
        let mut out: Vec<BrushMeta> = brushes
            .keys()
            .filter_map(|key| {
                let entry = match brushes.get(&key)? {
                    ValueOrContainer::Container(c) => c.into_map().ok()?,
                    ValueOrContainer::Value(_) => return None,
                };
                let get = |k: &str| entry.get(k);
                Some(BrushMeta {
                    id: BrushId(key.to_string()),
                    name: match get("name")? {
                        ValueOrContainer::Value(LoroValue::String(s)) => s.to_string(),
                        _ => return None,
                    },
                    spec: match get("spec")? {
                        ValueOrContainer::Value(LoroValue::Binary(b)) => b.to_vec(),
                        _ => return None,
                    },
                    updated_ms: match get("updated")? {
                        ValueOrContainer::Value(LoroValue::I64(v)) => v as u64,
                        _ => 0,
                    },
                })
            })
            .collect();
        out.sort_by_key(|b| std::cmp::Reverse(b.updated_ms));
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
    fn brush_library_roundtrip() {
        let a = WorkspaceDoc::new();
        let spec = crate::BrushSpec::preset(crate::Tool::Pencil)
            .encode()
            .unwrap();
        let soft = BrushMeta {
            id: BrushId("user:soft".into()),
            name: "Soft".into(),
            spec: spec.clone(),
            updated_ms: 5,
        };
        a.upsert_brush(&soft).unwrap();

        let b = WorkspaceDoc::new();
        b.import_update(&a.export_updates_since(&[]).unwrap())
            .unwrap();
        assert_eq!(b.brushes(), vec![soft.clone()]);
        assert_eq!(
            crate::BrushSpec::decode(&b.brushes()[0].spec)
                .unwrap()
                .tip
                .hardness,
            0.7
        );

        // Re-upsert edits in place; newest first; delete syncs.
        b.upsert_brush(&BrushMeta {
            name: "Softer".into(),
            updated_ms: 9,
            ..soft.clone()
        })
        .unwrap();
        b.upsert_brush(&BrushMeta {
            id: BrushId("user:old".into()),
            name: "Old".into(),
            spec,
            updated_ms: 1,
        })
        .unwrap();
        a.import_update(&b.export_updates_since(&a.version()).unwrap())
            .unwrap();
        let names: Vec<String> = a.brushes().into_iter().map(|b| b.name).collect();
        assert_eq!(names, ["Softer", "Old"]);
        a.remove_brush(&soft.id).unwrap();
        a.remove_brush(&BrushId("user:none".into())).unwrap();
        b.import_update(&a.export_updates_since(&b.version()).unwrap())
            .unwrap();
        assert_eq!(b.brushes().len(), 1);
    }

    #[test]
    fn device_registry_roundtrip() {
        let a = WorkspaceDoc::new();
        let ipad = DeviceMeta {
            id: crate::DeviceId::new(),
            name: "iPad".into(),
            platform: "iPad16,3".into(),
            last_seen_ms: 10,
        };
        a.upsert_device(&ipad).unwrap();

        let b = WorkspaceDoc::new();
        b.import_update(&a.export_updates_since(&[]).unwrap())
            .unwrap();
        let desk = DeviceMeta {
            id: crate::DeviceId::new(),
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
        assert_eq!(a.devices()[0].id, ipad.id);

        // Removal syncs; removing an unknown id is a no-op, not an error.
        // Bring b up to date first: a delete concurrent with a's re-upsert
        // above would resolve by peer id, which is random per doc.
        b.import_update(&a.export_updates_since(&b.version()).unwrap())
            .unwrap();
        b.remove_device(ipad.id).unwrap();
        b.remove_device(crate::DeviceId::new()).unwrap();
        a.import_update(&b.export_updates_since(&a.version()).unwrap())
            .unwrap();
        assert_eq!(a.devices().len(), 1);
        assert_eq!(a.devices()[0].id, desk.id);
    }
}
