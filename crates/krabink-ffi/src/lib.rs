//! UniFFI surface exposing krabink-core to Swift.
//!
//! Swift-facing objects: [`Core`] (store + note registry + the device's
//! sync node) and [`NoteSession`] (one open note). Events arrive through the foreign
//! traits [`CoreListener`] and [`NoteListener`]; implementations hop to
//! `@MainActor` on the Swift side.

uniffi::setup_scaffolding!("krabink");

mod brush;
mod engine;
mod markdown;
mod net;
mod types;

pub use brush::{
    AssetInfo, AssetKind, Blend, BrushKnobs, BrushModeler, BrushRef, BuiltinBrush, CustomBrush,
    INK_VERTEX_FLOATS, InkMesh, InkStyle, InputModel, MaskStyle, Overlap, RawSample, Recording,
    StrokeEnd, StrokeMetrics, builtin_assets, builtin_brushes, ism_available, measure_recording,
    parse_recording, resolve_page_bindings, rotate_shape, scale_shape, shape_frame,
};
pub use engine::{Core, CoreListener, KrabinkError, NoteListener, NoteSession};
pub use markdown::{
    PreviewText, StyleKind, StyleRun, inline_min_height, inline_padding, preview_text,
    sketch_embed_title, style_runs,
};
pub use types::{
    AppearanceInfo, Binding, BrushInfo, DeviceInfo, Element, ElementOrigin, Frame, NoteInfo,
    PageElement, PageMove, PageProbe, PairInfo, PeerInfo, PeerKind, Point2, PointKind, Recognition,
    Route, Shape, ShapeElement, Stroke, StrokePoint, SyncState, Tilt, Tool, build_pair_uri,
    parse_pair_uri,
};
