//! UniFFI surface exposing pendant-core to Swift.
//!
//! Swift-facing objects: [`Core`] (store + note registry + background sync)
//! and [`NoteSession`] (one open note). Events arrive through the foreign
//! traits [`CoreListener`] and [`NoteListener`]; implementations hop to
//! `@MainActor` on the Swift side.

uniffi::setup_scaffolding!("pendant");

mod brush;
mod engine;
mod net;
mod types;

pub use brush::{
    Blend, BrushModeler, BrushRef, INK_VERTEX_FLOATS, InkMesh, InkStyle, Overlap, RawSample,
    StrokeEnd,
};
pub use engine::{Core, CoreListener, NoteListener, NoteSession, PendantError};
pub use types::{
    Binding, Element, NoteInfo, Point2, PointKind, Recognition, Shape, ShapeElement, Stroke,
    StrokePoint, SyncState, Tilt, Tool,
};
