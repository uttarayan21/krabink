//! A semantics-free CRDT document: just import/export/version. The server
//! relays and persists docs without caring whether they are notes or the
//! workspace registry, so this is all it needs.

use loro::{ExportMode, LoroDoc};

use crate::Result;

pub struct SyncDoc {
    doc: LoroDoc,
}

impl SyncDoc {
    pub fn new() -> Self {
        Self {
            doc: LoroDoc::new(),
        }
    }

    /// Rebuild from stored bytes ([`crate::StoredDoc`]).
    pub fn from_bytes<'a>(
        snapshot: Option<&[u8]>,
        updates: impl IntoIterator<Item = &'a [u8]>,
    ) -> Result<Self> {
        let doc = Self::new();
        if let Some(snapshot) = snapshot {
            doc.doc.import(snapshot)?;
        }
        for update in updates {
            doc.doc.import(update)?;
        }
        Ok(doc)
    }

    /// Encoded version vector of everything this doc has seen.
    pub fn version(&self) -> Vec<u8> {
        self.doc.oplog_vv().encode()
    }

    /// Incremental update with everything the peer at `since` lacks.
    /// `since = &[]` (or garbage) falls back to the full history.
    pub fn export_updates_since(&self, since: &[u8]) -> Result<Vec<u8>> {
        let from = loro::VersionVector::decode(since).unwrap_or_default();
        Ok(self.doc.export(ExportMode::updates(&from))?)
    }

    pub fn export_snapshot(&self) -> Result<Vec<u8>> {
        Ok(self.doc.export(ExportMode::Snapshot)?)
    }

    /// Import a remote update. Returns whether it added anything the doc
    /// did not already have (false = duplicate, safe to not re-broadcast).
    pub fn import_update(&self, bytes: &[u8]) -> Result<bool> {
        let status = self.doc.import(bytes)?;
        Ok(!status.success.is_empty())
    }
}

impl Default for SyncDoc {
    fn default() -> Self {
        Self::new()
    }
}
