//! Core document model for pendant: CRDT-backed markdown notes with embedded
//! vector sketches, a sans-io sync protocol, and shared client/server storage.
//!
//! This crate is intentionally free of async runtimes and UI dependencies so
//! it compiles unchanged for Linux, macOS and iOS (via `pendant-ffi`).
//!
//! ```
//! use pendant_core::{NoteDoc, NoteId};
//!
//! let id = NoteId::new();
//! let a = NoteDoc::new(id);
//! let b = NoteDoc::new(id);
//! a.splice_text(0, 0, "# shared note").unwrap();
//! b.import_update(&a.export_updates_since(&[]).unwrap()).unwrap();
//! assert_eq!(b.text(), "# shared note");
//! ```

mod export;
mod geom;
mod ids;
mod note;
mod pair;
mod store;
mod stroke;
mod sync;
mod sync_doc;
mod wetink;
mod workspace;

pub use export::{ExportAsset, ExportBundle, SKETCH_URI_PREFIX, strokes_to_svg};
pub use geom::{RibbonMesh, flatten_stroke, ribbon};
pub use ids::{DeviceId, NoteId, SketchId, StrokeId};
pub use note::NoteDoc;
pub use pair::PairInfo;
pub use store::{DocKey, Flush, Store, StoredDoc};
pub use stroke::{
    PointKind, PointSize, Rgba, Stroke, StrokePoint, Tilt, Tool, decode_chunks, encode_chunks,
};
pub use sync::{
    CatchUp, ClientDocs, ClientEffect, ClientMsg, ClientSession, DocProvider, ErrorCode,
    PROTO_VERSION, ServerEffect, ServerMsg, ServerSession,
};
pub use sync_doc::SyncDoc;
pub use wetink::{WetInk, WetPoint};
pub use workspace::{NoteMeta, WorkspaceDoc};

/// Errors produced by the core document model.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("malformed id: {0}")]
    MalformedId(String),
    #[error("crdt error")]
    Crdt(#[from] loro::LoroError),
    #[error("crdt encode error")]
    Encode(#[from] loro::LoroEncodeError),
    #[error("storage error")]
    Storage(#[from] redb::Error),
    #[error("codec error")]
    Codec(#[from] postcard::Error),
    #[error("unknown sketch {0}")]
    UnknownSketch(SketchId),
    #[error("unexpected document shape: {0}")]
    Schema(String),
    #[error("protocol violation: {0}")]
    Protocol(String),
}

pub type Result<T, E = Error> = core::result::Result<T, E>;
