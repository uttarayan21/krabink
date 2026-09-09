//! FFI mirror types. Plain data records/enums crossing the UniFFI boundary;
//! ids are ULID strings, colors are RGBA8 packed big-endian into a u32.

use pendant_core as pcore;

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Tool {
    Pen,
    Marker,
    Monoline,
    /// Flat calligraphy nib oriented by azimuth + barrel roll.
    Brush,
}

impl From<Tool> for pcore::Tool {
    fn from(t: Tool) -> Self {
        match t {
            Tool::Pen => Self::Pen,
            Tool::Marker => Self::Marker,
            Tool::Monoline => Self::Monoline,
            Tool::Brush => Self::Brush,
        }
    }
}

impl From<pcore::Tool> for Tool {
    fn from(t: pcore::Tool) -> Self {
        match t {
            pcore::Tool::Pen => Self::Pen,
            pcore::Tool::Marker => Self::Marker,
            pcore::Tool::Monoline => Self::Monoline,
            pcore::Tool::Brush => Self::Brush,
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
            tilt: p.tilt.map(|t| pcore::Tilt {
                roll: t.roll,
                azimuth: t.azimuth,
                altitude: t.altitude,
            }),
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
            tilt: p.tilt.map(|t| Tilt {
                roll: t.roll,
                azimuth: t.azimuth,
                altitude: t.altitude,
            }),
            size: p.size.map(|s| PointSize { w: s.w, h: s.h }),
        }
    }
}

/// One live pen sample on the ephemeral wet-ink channel.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct WetPoint {
    pub x: f32,
    pub y: f32,
    pub force: f32,
    /// Rendered line width at this sample; receivers fall back to
    /// `base_width * force` when absent.
    pub width: Option<f32>,
    /// Flat-nib orientation (azimuth + roll, radians) for nib tools.
    pub nib: Option<f32>,
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
            color: rgba_from_u32(s.color),
            base_width: s.base_width,
            kind: s.kind.into(),
            points: s.points.into_iter().map(Into::into).collect(),
            created_ms: s.created_ms,
        }
    }
}

/// Renderers fall back to this width for wet ink whose points carry none.
const WET_WIDTH_FALLBACK: f32 = 2.0;

/// The committed stroke's ink as consistently wound triangles, flat
/// `[x0, y0, x1, y1, x2, y2, …]` in canvas units. Fill with the non-zero
/// rule. This is the desktop's exact geometry, so both platforms draw the
/// same ink.
#[uniffi::export]
pub fn stroke_triangles(stroke: Stroke) -> Vec<f32> {
    let stroke = pcore::Stroke::from(stroke);
    let flat = pcore::flatten_stroke(&stroke);
    flatten_xy(pcore::ribbon_triangles(
        stroke.tool,
        &flat,
        stroke.base_width,
    ))
}

/// The committed stroke's ink as one closed outline polygon, flat
/// `[x0, y0, x1, y1, …]`. Same geometry as [`stroke_triangles`] but a single
/// boundary, so path fillers antialias it cleanly. Fill non-zero.
#[uniffi::export]
pub fn stroke_outline(stroke: Stroke) -> Vec<f32> {
    let stroke = pcore::Stroke::from(stroke);
    let flat = pcore::flatten_stroke(&stroke);
    flatten_xy(pcore::ribbon_outline(stroke.tool, &flat, stroke.base_width))
}

/// Provisional (wet) ink outline for the points received so far.
#[uniffi::export]
pub fn wet_outline(points: Vec<WetPoint>, tool: Tool, base_width: f32) -> Vec<f32> {
    let flat: Vec<pcore::StrokePoint> = points.iter().map(wet_to_stroke_point).collect();
    flatten_xy(pcore::ribbon_outline(
        tool.into(),
        &flat,
        base_width.max(WET_WIDTH_FALLBACK),
    ))
}

/// Provisional (wet) ink for the points received so far, same geometry
/// as [`stroke_triangles`].
#[uniffi::export]
pub fn wet_triangles(points: Vec<WetPoint>, tool: Tool, base_width: f32) -> Vec<f32> {
    let flat: Vec<pcore::StrokePoint> = points.iter().map(wet_to_stroke_point).collect();
    flatten_xy(pcore::ribbon_triangles(
        tool.into(),
        &flat,
        base_width.max(WET_WIDTH_FALLBACK),
    ))
}

pub(crate) fn wet_to_stroke_point(p: &WetPoint) -> pcore::StrokePoint {
    pcore::StrokePoint {
        x: p.x,
        y: p.y,
        force: p.force,
        t_ms: 0,
        // Wet samples carry only the nib orientation; that is all a nib
        // tool needs to render.
        tilt: p.nib.map(|angle| pcore::Tilt {
            azimuth: angle,
            altitude: 0.0,
            roll: 0.0,
        }),
        size: p.width.map(|w| pcore::PointSize { w, h: w }),
    }
}

fn flatten_xy(tris: Vec<[f32; 2]>) -> Vec<f32> {
    tris.into_iter().flatten().collect()
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
            id: m.id,
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
