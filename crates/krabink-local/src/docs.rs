//! Server-side document registry: lazily loaded [`SyncDoc`]s over the shared
//! redb [`Store`], implementing the sans-io [`DocProvider`] contract.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use krabink_core::{CatchUp, DocKey, DocProvider, Flush, Result, Store, SyncDoc};

/// Compact a doc's update log once it grows past this many entries.
const COMPACT_AFTER_UPDATES: u64 = 1000;
/// Unload docs untouched for this long (state is safe in the store).
const IDLE_UNLOAD: Duration = Duration::from_secs(300);

struct OpenDoc {
    doc: SyncDoc,
    pending_updates: u64,
    last_used: Instant,
}

/// What [`ServerDocs::maintain`] does with docs nobody touched for
/// [`IDLE_UNLOAD`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleDocs {
    /// Drop them from memory; the store keeps their state.
    Unload,
    /// Keep them loaded (final checkpoint before exit).
    Keep,
}

/// All documents the server knows, plus their persistence.
pub struct ServerDocs {
    store: Store,
    open: HashMap<DocKey, OpenDoc>,
}

impl ServerDocs {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            open: HashMap::new(),
        }
    }

    /// Load `key` from the store if it isn't already open.
    fn ensure_open(&mut self, key: DocKey) -> Result<()> {
        if !self.open.contains_key(&key) {
            let stored = self.store.load(key)?;
            let doc = SyncDoc::from_bytes(
                stored.snapshot.as_deref(),
                stored.updates.iter().map(Vec::as_slice),
            )?;
            self.open.insert(
                key,
                OpenDoc {
                    doc,
                    pending_updates: self.store.update_count(key)?,
                    last_used: Instant::now(),
                },
            );
        }
        Ok(())
    }

    /// Checkpoint every open doc with pending updates and drop idle ones.
    /// Called periodically and on shutdown.
    pub fn maintain(&mut self, idle: IdleDocs) -> Result<()> {
        let now = Instant::now();
        let keys: Vec<DocKey> = self.open.keys().copied().collect();
        for key in keys {
            let entry = self.open.get_mut(&key).expect("key came from the map");
            if entry.pending_updates > 0 {
                let snapshot = entry.doc.export_snapshot()?;
                self.store.checkpoint(key, &snapshot)?;
                entry.pending_updates = 0;
            }
            if idle == IdleDocs::Unload && now.duration_since(entry.last_used) > IDLE_UNLOAD {
                self.open.remove(&key);
            }
        }
        Ok(())
    }
}

impl DocProvider for ServerDocs {
    fn catch_up(&mut self, doc: DocKey, have: &[u8]) -> Result<CatchUp> {
        self.ensure_open(doc)?;
        let entry = self.open.get_mut(&doc).expect("ensured above");
        entry.last_used = Instant::now();
        Ok(CatchUp {
            server_have: entry.doc.version(),
            payload: entry.doc.export_updates_since(have)?,
        })
    }

    fn import_update(&mut self, doc: DocKey, payload: &[u8]) -> Result<bool> {
        self.ensure_open(doc)?;
        let entry = self.open.get_mut(&doc).expect("ensured above");
        entry.last_used = Instant::now();
        if !entry.doc.import_update(payload)? {
            return Ok(false); // duplicate: nothing new to persist or relay
        }
        entry.pending_updates += 1;
        if entry.pending_updates >= COMPACT_AFTER_UPDATES {
            let snapshot = entry.doc.export_snapshot()?;
            self.store.checkpoint(doc, &snapshot)?;
            entry.pending_updates = 0;
        } else {
            // No fsync on the hot path; maintain() checkpoints durably.
            self.store.append_update(doc, payload, Flush::Eventual)?;
        }
        Ok(true)
    }

    /// Every doc on disk plus the open ones (a fresh doc may not have
    /// been checkpointed yet).
    fn list_docs(&mut self) -> Vec<DocKey> {
        let mut keys: Vec<DocKey> = self.store.keys().unwrap_or_default();
        for key in self.open.keys() {
            if !keys.contains(key) {
                keys.push(*key);
            }
        }
        keys
    }
}
