//! Snapshot + update-log persistence, shared by server, desktop, the web
//! build and (via FFI) the iPad app. CRDT-agnostic: it moves opaque byte
//! blobs.
//!
//! Two backends sit behind one [`Store`]: redb on native targets, and an
//! in-memory table set everywhere. The web build preloads the memory tables
//! from IndexedDB and mirrors every mutation back through a [`StoreSink`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::{NoteId, Result};

/// Canonical document identifier, used both as the storage key and on the
/// sync wire. Notes map from their id; the workspace doc has a reserved key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct DocKey(u128);

impl DocKey {
    /// The library/workspace registry doc.
    pub const WORKSPACE: Self = Self(0);

    pub const fn to_u128(self) -> u128 {
        self.0
    }

    pub const fn from_u128(raw: u128) -> Self {
        Self(raw)
    }
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

/// One mutation of a memory store, in the order it happened. Replaying the
/// ops into [`Store::memory_from`] reproduces the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreOp {
    /// Update `seq` of `key`'s log.
    Append {
        key: DocKey,
        seq: u64,
        bytes: Vec<u8>,
        flush: Flush,
    },
    /// Replace `key`'s snapshot and drop its whole update log.
    Checkpoint { key: DocKey, snapshot: Vec<u8> },
}

/// Receives every mutation of a memory store, after it is applied and in
/// order. Must not block: the web build forwards to an async writer.
pub trait StoreSink: Send + Sync {
    fn apply(&self, op: StoreOp);
}

/// The full contents of a store, as preloaded into [`Store::memory_from`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StoreImage {
    pub snapshots: Vec<(DocKey, Vec<u8>)>,
    pub updates: Vec<(DocKey, u64, Vec<u8>)>,
}

/// Document store: a redb file (native) or memory tables with an optional
/// write-behind sink.
pub struct Store {
    backend: Backend,
}

enum Backend {
    #[cfg(not(target_arch = "wasm32"))]
    Redb(redb_backend::RedbStore),
    Memory(MemStore),
}

impl Store {
    /// Open (or create) a single-file redb store.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open(path: &std::path::Path) -> Result<Self> {
        Ok(Self {
            backend: Backend::Redb(redb_backend::RedbStore::open(path)?),
        })
    }

    /// An empty store that lives only in memory.
    pub fn memory() -> Self {
        Self::memory_from(StoreImage::default(), None)
    }

    /// A memory store seeded from `image`; every later mutation is reported
    /// to `sink`.
    pub fn memory_from(image: StoreImage, sink: Option<Arc<dyn StoreSink>>) -> Self {
        let mut tables = MemTables::default();
        for (key, bytes) in image.snapshots {
            tables.snapshots.insert(key.0, bytes);
        }
        for (key, seq, bytes) in image.updates {
            tables.updates.insert((key.0, seq), bytes);
        }
        Self {
            backend: Backend::Memory(MemStore {
                tables: Mutex::new(tables),
                sink,
            }),
        }
    }

    pub fn load(&self, key: DocKey) -> Result<StoredDoc> {
        match &self.backend {
            #[cfg(not(target_arch = "wasm32"))]
            Backend::Redb(db) => db.load(key),
            Backend::Memory(mem) => Ok(mem.load(key)),
        }
    }

    /// Every doc with a snapshot or at least one update on disk.
    pub fn keys(&self) -> Result<Vec<DocKey>> {
        match &self.backend {
            #[cfg(not(target_arch = "wasm32"))]
            Backend::Redb(db) => db.keys(),
            Backend::Memory(mem) => Ok(mem.keys()),
        }
    }

    /// Append one incremental update to a doc's log.
    pub fn append_update(&self, key: DocKey, update: &[u8], flush: Flush) -> Result<()> {
        match &self.backend {
            #[cfg(not(target_arch = "wasm32"))]
            Backend::Redb(db) => db.append_update(key, update, flush),
            Backend::Memory(mem) => {
                mem.append_update(key, update, flush);
                Ok(())
            }
        }
    }

    /// Replace the stored snapshot and truncate the update log, atomically.
    pub fn checkpoint(&self, key: DocKey, snapshot: &[u8]) -> Result<()> {
        match &self.backend {
            #[cfg(not(target_arch = "wasm32"))]
            Backend::Redb(db) => db.checkpoint(key, snapshot),
            Backend::Memory(mem) => {
                mem.checkpoint(key, snapshot);
                Ok(())
            }
        }
    }

    /// Number of pending updates in a doc's log (drives compaction policy).
    pub fn update_count(&self, key: DocKey) -> Result<u64> {
        match &self.backend {
            #[cfg(not(target_arch = "wasm32"))]
            Backend::Redb(db) => db.update_count(key),
            Backend::Memory(mem) => Ok(mem.update_count(key)),
        }
    }
}

#[derive(Default)]
struct MemTables {
    snapshots: BTreeMap<u128, Vec<u8>>,
    updates: BTreeMap<(u128, u64), Vec<u8>>,
}

struct MemStore {
    tables: Mutex<MemTables>,
    sink: Option<Arc<dyn StoreSink>>,
}

impl MemStore {
    fn tables(&self) -> MutexGuard<'_, MemTables> {
        // A panic mid-mutation leaves plain maps behind; still consistent.
        self.tables
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn load(&self, key: DocKey) -> StoredDoc {
        let tables = self.tables();
        StoredDoc {
            snapshot: tables.snapshots.get(&key.0).cloned(),
            updates: tables
                .updates
                .range((key.0, 0)..=(key.0, u64::MAX))
                .map(|(_, v)| v.clone())
                .collect(),
        }
    }

    fn keys(&self) -> Vec<DocKey> {
        let tables = self.tables();
        tables
            .snapshots
            .keys()
            .copied()
            .chain(tables.updates.keys().map(|(k, _)| *k))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(DocKey)
            .collect()
    }

    fn append_update(&self, key: DocKey, update: &[u8], flush: Flush) {
        let mut tables = self.tables();
        let seq = tables
            .updates
            .range((key.0, 0)..=(key.0, u64::MAX))
            .next_back()
            .map_or(0, |((_, seq), _)| seq + 1);
        tables.updates.insert((key.0, seq), update.to_vec());
        // Report under the lock so concurrent writers reach the sink in
        // the order they hit the tables.
        if let Some(sink) = &self.sink {
            sink.apply(StoreOp::Append {
                key,
                seq,
                bytes: update.to_vec(),
                flush,
            });
        }
    }

    fn checkpoint(&self, key: DocKey, snapshot: &[u8]) {
        let mut tables = self.tables();
        tables.snapshots.insert(key.0, snapshot.to_vec());
        let seqs: Vec<_> = tables
            .updates
            .range((key.0, 0)..=(key.0, u64::MAX))
            .map(|(k, _)| *k)
            .collect();
        for k in seqs {
            tables.updates.remove(&k);
        }
        if let Some(sink) = &self.sink {
            sink.apply(StoreOp::Checkpoint {
                key,
                snapshot: snapshot.to_vec(),
            });
        }
    }

    fn update_count(&self, key: DocKey) -> u64 {
        self.tables()
            .updates
            .range((key.0, 0)..=(key.0, u64::MAX))
            .count() as u64
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod redb_backend {
    use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};

    use super::{DocKey, Flush, StoredDoc};
    use crate::Result;

    const SNAPSHOTS: TableDefinition<u128, &[u8]> = TableDefinition::new("snapshots");
    const UPDATES: TableDefinition<(u128, u64), &[u8]> = TableDefinition::new("updates");

    fn durability(flush: Flush) -> Durability {
        match flush {
            Flush::Eventual => Durability::None,
            Flush::Immediate => Durability::Immediate,
        }
    }

    pub(super) struct RedbStore {
        db: Database,
    }

    impl RedbStore {
        pub(super) fn open(path: &std::path::Path) -> Result<Self> {
            let db = Database::create(path).map_err(redb::Error::from)?;
            // Ensure tables exist so read paths never special-case first run.
            let txn = db.begin_write().map_err(redb::Error::from)?;
            txn.open_table(SNAPSHOTS).map_err(redb::Error::from)?;
            txn.open_table(UPDATES).map_err(redb::Error::from)?;
            txn.commit().map_err(redb::Error::from)?;
            Ok(Self { db })
        }

        pub(super) fn load(&self, key: DocKey) -> Result<StoredDoc> {
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
        pub(super) fn keys(&self) -> Result<Vec<DocKey>> {
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
            Ok(keys.into_iter().map(DocKey::from_u128).collect())
        }

        /// Append one incremental update to a doc's log.
        pub(super) fn append_update(&self, key: DocKey, update: &[u8], flush: Flush) -> Result<()> {
            let mut txn = self.db.begin_write().map_err(redb::Error::from)?;
            txn.set_durability(durability(flush))
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
        pub(super) fn checkpoint(&self, key: DocKey, snapshot: &[u8]) -> Result<()> {
            let mut txn = self.db.begin_write().map_err(redb::Error::from)?;
            txn.set_durability(durability(Flush::Immediate))
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
        pub(super) fn update_count(&self, key: DocKey) -> Result<u64> {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::note::NoteDoc;

    fn conformance(store: &Store) {
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
        assert_eq!(store.keys().unwrap(), vec![key]);
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

        // the log restarts after a checkpoint and other docs stay apart
        store.append_update(key, b"a", Flush::Eventual).unwrap();
        store
            .append_update(DocKey::WORKSPACE, b"w", Flush::Eventual)
            .unwrap();
        assert_eq!(store.update_count(key).unwrap(), 1);
        assert_eq!(store.load(DocKey::WORKSPACE).unwrap().updates, vec![b"w"]);
        assert_eq!(store.keys().unwrap().len(), 2);
    }

    #[test]
    fn redb_conformance() {
        let dir = tempfile::tempdir().unwrap();
        conformance(&Store::open(&dir.path().join("test.redb")).unwrap());
    }

    #[test]
    fn memory_conformance() {
        conformance(&Store::memory());
    }

    /// Folds ops into an image the way the IndexedDB writer does.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<StoreOp>>);

    impl StoreSink for Recorder {
        fn apply(&self, op: StoreOp) {
            self.0.lock().unwrap().push(op);
        }
    }

    fn replay(ops: &[StoreOp]) -> StoreImage {
        let mut snapshots = BTreeMap::new();
        let mut updates = BTreeMap::new();
        for op in ops {
            match op {
                StoreOp::Append {
                    key, seq, bytes, ..
                } => {
                    updates.insert((key.0, *seq), bytes.clone());
                }
                StoreOp::Checkpoint { key, snapshot } => {
                    snapshots.insert(key.0, snapshot.clone());
                    updates.retain(|(k, _), _| *k != key.0);
                }
            }
        }
        StoreImage {
            snapshots: snapshots.into_iter().map(|(k, v)| (DocKey(k), v)).collect(),
            updates: updates
                .into_iter()
                .map(|((k, seq), v)| (DocKey(k), seq, v))
                .collect(),
        }
    }

    #[test]
    fn sink_replay_reproduces_store() {
        let recorder = Arc::new(Recorder::default());
        let store = Store::memory_from(StoreImage::default(), Some(recorder.clone()));
        conformance(&store);

        let ops = recorder.0.lock().unwrap().clone();
        let copy = Store::memory_from(replay(&ops), None);
        assert_eq!(copy.keys().unwrap(), store.keys().unwrap());
        for key in store.keys().unwrap() {
            let (a, b) = (store.load(key).unwrap(), copy.load(key).unwrap());
            assert_eq!(a.snapshot, b.snapshot);
            assert_eq!(a.updates, b.updates);
            // seqs continue from the preloaded log
            copy.append_update(key, b"next", Flush::Eventual).unwrap();
        }
        assert!(matches!(ops.last(), Some(StoreOp::Append { seq: 0, .. })));
    }
}
