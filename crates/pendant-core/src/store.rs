//! Snapshot + update-log persistence over redb, shared by server, desktop and
//! (via FFI) the iPad app. CRDT-agnostic: it moves opaque byte blobs.

use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};

use crate::{NoteId, Result};

const SNAPSHOTS: TableDefinition<u128, &[u8]> = TableDefinition::new("snapshots");
const UPDATES: TableDefinition<(u128, u64), &[u8]> = TableDefinition::new("updates");

/// Canonical document identifier, used both as the storage key and on the
/// sync wire. Notes map from their id; the workspace doc has a reserved key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct DocKey(u128);

impl DocKey {
    /// The library/workspace registry doc.
    pub const WORKSPACE: Self = Self(0);
}

impl From<NoteId> for DocKey {
    fn from(id: NoteId) -> Self {
        Self(u128::from_be_bytes(
            id.to_string()
                .parse::<ulid::Ulid>()
                .expect("NoteId is always a valid ulid")
                .to_bytes(),
        ))
    }
}

/// How urgently a write must reach disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flush {
    /// No fsync; the write becomes durable with the next `Immediate` commit.
    /// Used for the high-frequency path while drawing/typing - a pen-up or
    /// doc-close flush always follows.
    Eventual,
    /// Fsync before returning; used on pen-up, doc close and app background.
    Immediate,
}

impl Flush {
    fn durability(self) -> Durability {
        match self {
            Self::Eventual => Durability::None,
            Self::Immediate => Durability::Immediate,
        }
    }
}

/// A document as loaded from disk: latest snapshot plus updates since.
#[derive(Debug, Default)]
pub struct StoredDoc {
    pub snapshot: Option<Vec<u8>>,
    pub updates: Vec<Vec<u8>>,
}

impl StoredDoc {
    pub fn is_empty(&self) -> bool {
        self.snapshot.is_none() && self.updates.is_empty()
    }
}

/// Single-file document store.
pub struct Store {
    db: Database,
}

impl Store {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let db = Database::create(path).map_err(redb::Error::from)?;
        // Ensure tables exist so read paths never special-case first run.
        let txn = db.begin_write().map_err(redb::Error::from)?;
        txn.open_table(SNAPSHOTS).map_err(redb::Error::from)?;
        txn.open_table(UPDATES).map_err(redb::Error::from)?;
        txn.commit().map_err(redb::Error::from)?;
        Ok(Self { db })
    }

    pub fn load(&self, key: DocKey) -> Result<StoredDoc> {
        let txn = self.db.begin_read().map_err(redb::Error::from)?;

        let snapshot = txn
            .open_table(SNAPSHOTS)
            .map_err(redb::Error::from)?
            .get(key.0)
            .map_err(redb::Error::from)?
            .map(|v| v.value().to_vec());

        let updates = txn
            .open_table(UPDATES)
            .map_err(redb::Error::from)?
            .range((key.0, 0)..=(key.0, u64::MAX))
            .map_err(redb::Error::from)?
            .map(|entry| {
                entry
                    .map(|(_, v)| v.value().to_vec())
                    .map_err(redb::Error::from)
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(StoredDoc { snapshot, updates })
    }

    /// Every doc with a snapshot or at least one update on disk.
    pub fn keys(&self) -> Result<Vec<DocKey>> {
        let txn = self.db.begin_read().map_err(redb::Error::from)?;
        let mut keys = std::collections::BTreeSet::new();
        for entry in txn
            .open_table(SNAPSHOTS)
            .map_err(redb::Error::from)?
            .iter()
            .map_err(redb::Error::from)?
        {
            let (k, _) = entry.map_err(redb::Error::from)?;
            keys.insert(k.value());
        }
        for entry in txn
            .open_table(UPDATES)
            .map_err(redb::Error::from)?
            .iter()
            .map_err(redb::Error::from)?
        {
            let (k, _) = entry.map_err(redb::Error::from)?;
            keys.insert(k.value().0);
        }
        Ok(keys.into_iter().map(DocKey).collect())
    }

    /// Append one incremental update to a doc's log.
    pub fn append_update(&self, key: DocKey, update: &[u8], flush: Flush) -> Result<()> {
        let mut txn = self.db.begin_write().map_err(redb::Error::from)?;
        txn.set_durability(flush.durability())
            .map_err(redb::Error::from)?;
        {
            let mut table = txn.open_table(UPDATES).map_err(redb::Error::from)?;
            let next_seq = table
                .range((key.0, 0)..=(key.0, u64::MAX))
                .map_err(redb::Error::from)?
                .next_back()
                .transpose()
                .map_err(redb::Error::from)?
                .map(|(k, _)| k.value().1 + 1)
                .unwrap_or(0);
            table
                .insert((key.0, next_seq), update)
                .map_err(redb::Error::from)?;
        }
        txn.commit().map_err(redb::Error::from)?;
        Ok(())
    }

    /// Replace the stored snapshot and truncate the update log, atomically.
    pub fn checkpoint(&self, key: DocKey, snapshot: &[u8]) -> Result<()> {
        let mut txn = self.db.begin_write().map_err(redb::Error::from)?;
        txn.set_durability(Flush::Immediate.durability())
            .map_err(redb::Error::from)?;
        {
            let mut snapshots = txn.open_table(SNAPSHOTS).map_err(redb::Error::from)?;
            snapshots
                .insert(key.0, snapshot)
                .map_err(redb::Error::from)?;
            let mut updates = txn.open_table(UPDATES).map_err(redb::Error::from)?;
            updates
                .retain_in((key.0, 0)..=(key.0, u64::MAX), |_, _| false)
                .map_err(redb::Error::from)?;
        }
        txn.commit().map_err(redb::Error::from)?;
        Ok(())
    }

    /// Number of pending updates in a doc's log (drives compaction policy).
    pub fn update_count(&self, key: DocKey) -> Result<u64> {
        let txn = self.db.begin_read().map_err(redb::Error::from)?;
        let count = txn
            .open_table(UPDATES)
            .map_err(redb::Error::from)?
            .range((key.0, 0)..=(key.0, u64::MAX))
            .map_err(redb::Error::from)?
            .count() as u64;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::note::NoteDoc;

    #[test]
    fn reload_equals_live_doc() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.redb")).unwrap();

        let id = NoteId::new();
        let key = DocKey::from(id);
        let doc = NoteDoc::new(id);

        let mut vv = doc.version();
        doc.splice_text(0, 0, "hello").unwrap();
        store
            .append_update(
                key,
                &doc.export_updates_since(&vv).unwrap(),
                Flush::Eventual,
            )
            .unwrap();
        vv = doc.version();
        doc.splice_text(5, 0, " world").unwrap();
        store
            .append_update(
                key,
                &doc.export_updates_since(&vv).unwrap(),
                Flush::Immediate,
            )
            .unwrap();

        let stored = store.load(key).unwrap();
        assert_eq!(stored.updates.len(), 2);
        let reloaded = NoteDoc::from_bytes(
            id,
            stored.snapshot.as_deref(),
            stored.updates.iter().map(Vec::as_slice),
        )
        .unwrap();
        assert_eq!(reloaded.text(), doc.text());

        // checkpoint compacts the log without losing state
        store
            .checkpoint(key, &doc.export_snapshot().unwrap())
            .unwrap();
        assert_eq!(store.update_count(key).unwrap(), 0);
        let stored = store.load(key).unwrap();
        let reloaded = NoteDoc::from_bytes(
            id,
            stored.snapshot.as_deref(),
            stored.updates.iter().map(Vec::as_slice),
        )
        .unwrap();
        assert_eq!(reloaded.text(), "hello world");
    }
}
