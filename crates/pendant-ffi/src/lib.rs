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
    AssetInfo, AssetKind, Blend, BrushKnobs, BrushModeler, BrushRef, BuiltinBrush, CustomBrush,
    INK_VERTEX_FLOATS, InkMesh, InkStyle, InputModel, MaskStyle, Overlap, RawSample, Recording,
    StrokeEnd, StrokeMetrics, builtin_assets, builtin_brushes, ism_available, measure_recording,
    parse_recording,
};
pub use engine::{Core, CoreListener, NoteListener, NoteSession, PendantError};
pub use types::{
    Binding, BrushInfo, Element, NoteInfo, Point2, PointKind, Recognition, Shape, ShapeElement,
    Stroke, StrokePoint, SyncState, Tilt, Tool,
};
