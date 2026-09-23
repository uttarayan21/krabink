//! UniFFI surface exposing krabink-core to Swift.
//!
//! Swift-facing objects: [`Core`] (store + note registry + the device's
//! sync node) and [`NoteSession`] (one open note). Events arrive through the foreign
//! traits [`CoreListener`] and [`NoteListener`]; implementations hop to
//! `@MainActor` on the Swift side.

uniffi::setup_scaffolding!("krabink");

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
pub use engine::{Core, CoreListener, KrabinkError, NoteListener, NoteSession};
pub use types::{
    Binding, BrushInfo, DeviceInfo, Element, NoteInfo, PairInfo, PeerInfo, PeerKind, Point2,
    PointKind, Recognition, Route, Shape, ShapeElement, Stroke, StrokePoint, SyncState, Tilt, Tool,
    build_pair_uri, parse_pair_uri,
};
