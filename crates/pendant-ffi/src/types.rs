//! FFI mirror types. Plain data records/enums crossing the UniFFI boundary;
//! ids are ULID strings, colors are RGBA8 packed big-endian into a u32.

use pendant_core as pcore;

/// A built-in brush preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Tool {
    Pen,
    /// Graphite: tilt widens and lightens, pressure darkens.
    Pencil,
    /// Chisel highlighter: translucent, never darker where it crosses
    /// itself.
    Marker,
    Monoline,
    /// Flat calligraphy nib oriented by azimuth + barrel roll.
    Fountain,
}

impl From<Tool> for pcore::Tool {
    fn from(t: Tool) -> Self {
        match t {
            Tool::Pen => Self::Pen,
            Tool::Pencil => Self::Pencil,
            Tool::Marker => Self::Marker,
            Tool::Monoline => Self::Monoline,
            Tool::Fountain => Self::Fountain,
        }
    }
}

impl From<pcore::Tool> for Tool {
    fn from(t: pcore::Tool) -> Self {
        match t {
            pcore::Tool::Pen => Self::Pen,
            pcore::Tool::Pencil => Self::Pencil,
            pcore::Tool::Marker => Self::Marker,
            pcore::Tool::Monoline => Self::Monoline,
            pcore::Tool::Fountain => Self::Fountain,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PointKind {
    BsplineControl,
    PolylineSample,
}

impl From<PointKind> for pcore::PointKind {
    fn from(k: PointKind) -> Self {
        match k {
            PointKind::BsplineControl => Self::BSplineControl,
            PointKind::PolylineSample => Self::PolylineSample,
        }
    }
}

impl From<pcore::PointKind> for PointKind {
    fn from(k: pcore::PointKind) -> Self {
        match k {
            pcore::PointKind::BSplineControl => Self::BsplineControl,
            pcore::PointKind::PolylineSample => Self::PolylineSample,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct Tilt {
    /// Radians, 0..2π.
    pub azimuth: f32,
    /// Radians, 0 (flat) .. π/2 (perpendicular).
    pub altitude: f32,
    /// Barrel roll, radians -π..π (Apple Pencil Pro); 0 when unknown.
    pub roll: f32,
}

impl From<Tilt> for pcore::Tilt {
    fn from(t: Tilt) -> Self {
        Self {
            azimuth: t.azimuth,
            altitude: t.altitude,
            roll: t.roll,
        }
    }
}

impl From<pcore::Tilt> for Tilt {
    fn from(t: pcore::Tilt) -> Self {
        Self {
            azimuth: t.azimuth,
            altitude: t.altitude,
            roll: t.roll,
        }
    }
}

/// Rendered point size in canvas units (PencilKit `PKStrokePoint.size`).
/// PencilKit derives it from more than force, so strokes drop fidelity
/// without it.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct PointSize {
    pub w: f32,
    pub h: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct StrokePoint {
    pub x: f32,
    pub y: f32,
    /// Normalised pressure, 0..=1.
    pub force: f32,
    /// Milliseconds since stroke start.
    pub t_ms: u32,
    pub tilt: Option<Tilt>,
    pub size: Option<PointSize>,
}

impl From<StrokePoint> for pcore::StrokePoint {
    fn from(p: StrokePoint) -> Self {
        Self {
            x: p.x,
            y: p.y,
            force: p.force,
            t_ms: p.t_ms,
            tilt: p.tilt.map(Into::into),
            size: p.size.map(|s| pcore::PointSize { w: s.w, h: s.h }),
        }
    }
}

impl From<pcore::StrokePoint> for StrokePoint {
    fn from(p: pcore::StrokePoint) -> Self {
        Self {
            x: p.x,
            y: p.y,
            force: p.force,
            t_ms: p.t_ms,
            tilt: p.tilt.map(Into::into),
            size: p.size.map(|s| PointSize { w: s.w, h: s.h }),
        }
    }
}

/// A finished stroke, as stored in the CRDT.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct Stroke {
    /// ULID string; use the id returned by `begin_stroke` so receivers can
    /// swap provisional wet ink for this committed stroke.
    pub id: String,
    pub tool: Tool,
    /// RGBA8 packed big-endian: 0xRRGGBBAA.
    pub color: u32,
    pub base_width: f32,
    pub kind: PointKind,
    pub points: Vec<StrokePoint>,
    /// Unix millis at stroke creation.
    pub created_ms: u64,
}

impl From<Stroke> for pcore::Stroke {
    fn from(s: Stroke) -> Self {
        Self {
            id: s.id.parse().unwrap_or_else(|_| pcore::StrokeId::new()),
            tool: s.tool.into(),
            // Custom brushes cross the boundary once the brush library
            // lands; until then every stroke uses its tool's preset.
            brush: None,
            color: rgba_from_u32(s.color),
            base_width: s.base_width,
            kind: s.kind.into(),
            points: s.points.into_iter().map(Into::into).collect(),
            created_ms: s.created_ms,
        }
    }
}

impl From<pcore::Stroke> for Stroke {
    fn from(s: pcore::Stroke) -> Self {
        Self {
            id: s.id.to_string(),
            tool: s.tool.into(),
            color: rgba_to_u32(s.color),
            base_width: s.base_width,
            kind: s.kind.into(),
            points: s.points.into_iter().map(Into::into).collect(),
            created_ms: s.created_ms,
        }
    }
}

/// One row of the synced device registry.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub platform: String,
    /// Unix millis of the last time this device came online (not liveness).
    pub last_seen_ms: u64,
}

impl From<pcore::DeviceMeta> for DeviceInfo {
    fn from(m: pcore::DeviceMeta) -> Self {
        Self {
            id: m.id.to_string(),
            name: m.name,
            platform: m.platform,
            last_seen_ms: m.last_seen_ms,
        }
    }
}

/// Sync coordinates carried by a `pendant://pair` URI (QR pairing).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PairInfo {
    /// Preferred direct path: the sharing desktop's embedded relay.
    pub server: String,
    pub token: String,
    /// Dedicated relay to route through when no direct path is reachable.
    pub fallback: Option<String>,
    /// Further direct paths to the same desktop (other interfaces).
    pub alt: Vec<String>,
    /// The desktop's device id: match against the `id` TXT record of a
    /// browsed `_pendant._tcp` service to find it by mDNS.
    pub relay_id: Option<String>,
}

impl From<pcore::PairInfo> for PairInfo {
    fn from(p: pcore::PairInfo) -> Self {
        Self {
            server: p.server,
            token: p.token,
            fallback: p.fallback,
            alt: p.alt,
            relay_id: p.relay_id,
        }
    }
}

impl From<PairInfo> for pcore::PairInfo {
    fn from(p: PairInfo) -> Self {
        Self {
            server: p.server,
            token: p.token,
            fallback: p.fallback,
            alt: p.alt,
            relay_id: p.relay_id,
        }
    }
}

/// Build the pairing URI a client renders as a QR code.
#[uniffi::export]
pub fn build_pair_uri(info: PairInfo) -> String {
    pcore::PairInfo::from(info).to_uri()
}

/// Parse a scanned/opened pairing URI; `None` when it is not one of ours.
#[uniffi::export]
pub fn parse_pair_uri(uri: String) -> Option<PairInfo> {
    pcore::PairInfo::parse(&uri).map(Into::into)
}

/// Entry in the note registry (the workspace doc).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NoteInfo {
    pub id: String,
    pub title: String,
    pub archived: bool,
    pub updated_ms: u64,
}

impl From<pcore::NoteMeta> for NoteInfo {
    fn from(m: pcore::NoteMeta) -> Self {
        Self {
            id: m.id.to_string(),
            title: m.title,
            archived: m.archived,
            updated_ms: m.updated_ms,
        }
    }
}

/// Connection state reported to [`crate::CoreListener::sync_state`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum SyncState {
    Disconnected,
    Connecting,
    /// Handshake done; `url` is the path that won (direct or fallback).
    Connected {
        url: String,
    },
    /// Server rejected us; reconnecting without change is pointless.
    Fatal {
        message: String,
    },
}

pub(crate) fn rgba_from_u32(v: u32) -> pcore::Rgba {
    pcore::Rgba(v.to_be_bytes())
}

pub(crate) fn rgba_to_u32(c: pcore::Rgba) -> u32 {
    u32::from_be_bytes(c.0)
}

// ---- elements ----

#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct Point2 {
    pub x: f32,
    pub y: f32,
}

impl From<[f32; 2]> for Point2 {
    fn from([x, y]: [f32; 2]) -> Self {
        Self { x, y }
    }
}

impl From<Point2> for [f32; 2] {
    fn from(p: Point2) -> Self {
        [p.x, p.y]
    }
}

/// A recognised primitive in canvas space (x right, y down).
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Enum)]
pub enum Shape {
    Line {
        a: Point2,
        b: Point2,
    },
    /// Head at `b`.
    Arrow {
        a: Point2,
        b: Point2,
    },
    /// Full `size` rotated by `angle` radians about `center`.
    Rect {
        center: Point2,
        size: Point2,
        angle: f32,
    },
    /// Half axes `radii` rotated by `angle` radians about `center`.
    Ellipse {
        center: Point2,
        radii: Point2,
        angle: f32,
    },
}

impl From<pcore::Shape> for Shape {
    fn from(s: pcore::Shape) -> Self {
        match s {
            pcore::Shape::Line { a, b } => Self::Line {
                a: a.into(),
                b: b.into(),
            },
            pcore::Shape::Arrow { a, b } => Self::Arrow {
                a: a.into(),
                b: b.into(),
            },
            pcore::Shape::Rect {
                center,
                size,
                angle,
            } => Self::Rect {
                center: center.into(),
                size: size.into(),
                angle,
            },
            pcore::Shape::Ellipse {
                center,
                radii,
                angle,
            } => Self::Ellipse {
                center: center.into(),
                radii: radii.into(),
                angle,
            },
        }
    }
}

impl From<Shape> for pcore::Shape {
    fn from(s: Shape) -> Self {
        match s {
            Shape::Line { a, b } => Self::Line {
                a: a.into(),
                b: b.into(),
            },
            Shape::Arrow { a, b } => Self::Arrow {
                a: a.into(),
                b: b.into(),
            },
            Shape::Rect {
                center,
                size,
                angle,
            } => Self::Rect {
                center: center.into(),
                size: size.into(),
                angle,
            },
            Shape::Ellipse {
                center,
                radii,
                angle,
            } => Self::Ellipse {
                center: center.into(),
                radii: radii.into(),
                angle,
            },
        }
    }
}

/// A snapped shape and how cleanly it was drawn, 0 (barely) ..= 1.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct Recognition {
    pub shape: Shape,
    pub confidence: f32,
}

impl From<pcore::Recognition> for Recognition {
    fn from(r: pcore::Recognition) -> Self {
        Self {
            shape: r.shape.into(),
            confidence: r.confidence,
        }
    }
}

/// A line or arrow end attached to another element (Excalidraw
/// `fixedPoint`): `fixed_point` in the target's unit square, `gap` the
/// distance kept from its outline. Reserved; v1 never writes one.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct Binding {
    pub element: String,
    pub fixed_point: Point2,
    pub gap: f32,
}

impl From<pcore::Binding> for Binding {
    fn from(b: pcore::Binding) -> Self {
        Self {
            element: b.element.to_string(),
            fixed_point: b.fixed_point.into(),
            gap: b.gap,
        }
    }
}

impl TryFrom<Binding> for pcore::Binding {
    type Error = pcore::Error;

    fn try_from(b: Binding) -> Result<Self, Self::Error> {
        Ok(Self {
            element: b.element.parse()?,
            fixed_point: b.fixed_point.into(),
            gap: b.gap,
        })
    }
}

/// A recognised shape as stored: geometry plus the ink style a stroke
/// would carry.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct ShapeElement {
    /// ULID string; the id its wet ink streamed under, so receivers swap
    /// the provisional ink for the shape.
    pub id: String,
    pub shape: Shape,
    pub tool: Tool,
    /// RGBA8 packed big-endian: 0xRRGGBBAA.
    pub color: u32,
    /// Full ink width in canvas units.
    pub width: f32,
    pub start: Option<Binding>,
    pub end: Option<Binding>,
    /// Unix millis at creation.
    pub created_ms: u64,
}

impl From<pcore::ShapeElement> for ShapeElement {
    fn from(s: pcore::ShapeElement) -> Self {
        Self {
            id: s.id.to_string(),
            shape: s.shape.into(),
            tool: s.style.tool.into(),
            color: rgba_to_u32(s.style.color),
            width: s.style.width,
            start: s.start.map(Into::into),
            end: s.end.map(Into::into),
            created_ms: s.created_ms,
        }
    }
}

impl From<ShapeElement> for pcore::ShapeElement {
    fn from(s: ShapeElement) -> Self {
        Self {
            id: s.id.parse().unwrap_or_else(|_| pcore::ElementId::new()),
            shape: s.shape.into(),
            style: pcore::Style {
                tool: s.tool.into(),
                color: rgba_from_u32(s.color),
                width: s.width,
            },
            // A binding to an unparsable id is no binding.
            start: s.start.and_then(|b| b.try_into().ok()),
            end: s.end.and_then(|b| b.try_into().ok()),
            created_ms: s.created_ms,
        }
    }
}

/// One entry of a sketch's z-ordered element list.
#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum Element {
    Stroke(Stroke),
    Shape(ShapeElement),
}

impl Element {
    pub fn id(&self) -> &str {
        match self {
            Self::Stroke(s) => &s.id,
            Self::Shape(s) => &s.id,
        }
    }

    pub fn color(&self) -> u32 {
        match self {
            Self::Stroke(s) => s.color,
            Self::Shape(s) => s.color,
        }
    }
}

impl From<pcore::Element> for Element {
    fn from(e: pcore::Element) -> Self {
        match e {
            pcore::Element::Stroke(s) => Self::Stroke(s.into()),
            pcore::Element::Shape(s) => Self::Shape(s.into()),
        }
    }
}

impl From<Element> for pcore::Element {
    fn from(e: Element) -> Self {
        match e {
            Element::Stroke(s) => Self::Stroke(s.into()),
            Element::Shape(s) => Self::Shape(s.into()),
        }
    }
}
