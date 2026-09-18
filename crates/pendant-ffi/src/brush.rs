//! Brush model and mesh surface for the native ink renderer: raw touches in,
//! modelled points and indexed triangle meshes out. Everything above
//! "upload vertices, draw triangles" lives in `pendant-core` so the iPad and
//! the desktop draw the same ink (`docs/plans/ink-renderer.md`).

use std::sync::{Arc, Mutex, PoisonError};

use pendant_core as pcore;

use crate::types::{
    Element, Point2, Recognition, Shape, Stroke, StrokePoint, Tilt, Tool, rgba_from_u32,
    rgba_to_u32,
};

/// One raw touch sample, before smoothing.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct RawSample {
    pub x: f32,
    pub y: f32,
    /// Normalised pressure, 0..=1. Touches without pressure pass 0.5.
    pub force: f32,
    /// Milliseconds on any monotonic clock (`UITouch.timestamp * 1000`);
    /// only differences matter.
    pub t_ms: f64,
    pub tilt: Option<Tilt>,
    /// `UITouch.estimationUpdateIndex` when the platform may still revise
    /// `force` or `tilt`; patch the revision in with
    /// [`BrushModeler::update`].
    pub estimation_id: Option<u32>,
    /// Whether a revision is expected (`estimatedPropertiesExpectingUpdates`
    /// non-empty).
    pub expects_update: bool,
}

impl From<RawSample> for pcore::RawSample {
    fn from(s: RawSample) -> Self {
        Self {
            x: s.x,
            y: s.y,
            force: s.force,
            t_ms: s.t_ms,
            tilt: s.tilt.map(Into::into),
            estimate: s.estimation_id.map(|id| pcore::Estimate {
                id,
                pending: s.expects_update,
            }),
        }
    }
}

/// A brush applied at a size: the tool's preset, or a custom brush's spec
/// with the tool as what the stroke records.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct BrushRef {
    pub tool: Tool,
    /// Full ink width in canvas units.
    pub base_width: f32,
    /// Per-stroke randomness seed, the stroke id's low bits
    /// ([`stroke_seed`]); only stroke-mapped grain and stamps read it, so
    /// 0 is fine for ink that has no element yet.
    #[uniffi(default = 0)]
    pub seed: u32,
    /// A custom brush (bundled or from the library); `None` draws with
    /// `tool`'s preset. Its spec is what the stroke snapshots.
    #[uniffi(default = None)]
    pub custom: Option<CustomBrush>,
}

/// A custom brush by id with its encoded spec ([`BrushSpec::encode`] in
/// the core); strokes carry both so they render without the library.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CustomBrush {
    pub id: String,
    pub spec: Vec<u8>,
}

impl CustomBrush {
    /// The decoded spec, or `None` (with a warning) when the bytes are
    /// from a build this one cannot read; callers fall back to the
    /// tool's preset, as the document reader does.
    pub(crate) fn decoded(&self) -> Option<pcore::CustomBrush> {
        match pcore::BrushSpec::decode(&self.spec) {
            Ok(spec) => Some(pcore::CustomBrush {
                id: pcore::BrushId(self.id.clone()),
                spec,
            }),
            Err(err) => {
                tracing::warn!(%err, brush = %self.id, "unreadable brush spec; using the tool preset");
                None
            }
        }
    }
}

impl From<pcore::CustomBrush> for CustomBrush {
    fn from(b: pcore::CustomBrush) -> Self {
        Self {
            id: b.id.0,
            spec: b.spec.encode().unwrap_or_default(),
        }
    }
}

/// A brush bundled with the app, offered next to the tool presets.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct BuiltinBrush {
    /// `builtin:…`
    pub id: String,
    pub name: String,
    /// Encoded spec; pass it in [`CustomBrush::spec`].
    pub spec: Vec<u8>,
}

/// The numbers a brush editor exposes; `None` where the spec has no such
/// control. See the core's `BrushKnobs`.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct BrushKnobs {
    pub opacity: f32,
    pub hardness: f32,
    pub spacing: Option<f32>,
    pub scatter: Option<f32>,
    pub size_jitter: Option<f32>,
    pub opacity_jitter: Option<f32>,
    pub grain_strength: Option<f32>,
    pub grain_scale: Option<f32>,
}

impl From<pcore::BrushKnobs> for BrushKnobs {
    fn from(k: pcore::BrushKnobs) -> Self {
        Self {
            opacity: k.opacity,
            hardness: k.hardness,
            spacing: k.spacing,
            scatter: k.scatter,
            size_jitter: k.size_jitter,
            opacity_jitter: k.opacity_jitter,
            grain_strength: k.grain_strength,
            grain_scale: k.grain_scale,
        }
    }
}

impl From<BrushKnobs> for pcore::BrushKnobs {
    fn from(k: BrushKnobs) -> Self {
        Self {
            opacity: k.opacity,
            hardness: k.hardness,
            spacing: k.spacing,
            scatter: k.scatter,
            size_jitter: k.size_jitter,
            opacity_jitter: k.opacity_jitter,
            grain_strength: k.grain_strength,
            grain_scale: k.grain_scale,
        }
    }
}

/// The editable numbers of an encoded spec; `None` when it does not
/// decode in this build.
#[uniffi::export]
pub fn brush_knobs(spec: Vec<u8>) -> Option<BrushKnobs> {
    pcore::BrushSpec::decode(&spec)
        .ok()
        .map(|s| s.knobs().into())
}

/// `spec` with `knobs` written into it, re-encoded; the input unchanged
/// when it does not decode.
#[uniffi::export]
pub fn brush_with_knobs(spec: Vec<u8>, knobs: BrushKnobs) -> Vec<u8> {
    match pcore::BrushSpec::decode(&spec) {
        Ok(s) => s.with_knobs(&knobs.into()).encode().unwrap_or(spec),
        Err(_) => spec,
    }
}

/// Every brush bundled with the app (a crayon and a grainy pencil).
#[uniffi::export]
pub fn builtin_brushes() -> Vec<BuiltinBrush> {
    pcore::BrushSpec::builtins()
        .into_iter()
        .map(|b| BuiltinBrush {
            id: b.id.0,
            name: b.name.to_owned(),
            spec: b.spec.encode().unwrap_or_default(),
        })
        .collect()
}

impl BrushRef {
    fn ink(&self, color: u32) -> pcore::Ink<'static> {
        let color = rgba_from_u32(color);
        match self.custom.as_ref().and_then(CustomBrush::decoded) {
            Some(custom) => pcore::Ink::custom(custom.spec, color, self.base_width),
            None => pcore::Ink::preset(self.tool.into(), color, self.base_width),
        }
        .with_seed(self.seed)
    }
}

/// The seed a stroke or shape with element id `id` renders with; 0 for
/// an id that does not parse.
#[uniffi::export]
pub fn stroke_seed(id: String) -> u32 {
    id.parse::<pcore::ElementId>().map_or(0, |id| id.seed())
}

/// Whether a run of points is still being drawn or is the whole stroke;
/// only a finished stroke gets its end taper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum StrokeEnd {
    Live,
    Complete,
}

impl From<StrokeEnd> for pcore::StrokeEnd {
    fn from(e: StrokeEnd) -> Self {
        match e {
            StrokeEnd::Live => Self::Live,
            StrokeEnd::Complete => Self::Complete,
        }
    }
}

/// One live stroke's input pipeline: streamline smoothing into the points
/// the stroke stores. Create at pen-down, `push` every coalesced touch,
/// draw `points` plus a `predict` tail each frame through [`points_mesh`],
/// commit `finish` at pen-up. Thread-safe; the touch handler and the
/// render loop may share it.
#[derive(uniffi::Object)]
pub struct BrushModeler {
    inner: Mutex<pcore::BrushModeler>,
}

impl BrushModeler {
    fn lock(&self) -> std::sync::MutexGuard<'_, pcore::BrushModeler> {
        // The modeler mutates nothing across a panic boundary that a later
        // call could observe half-done; continue rather than poison.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[uniffi::export]
impl BrushModeler {
    /// A modeler for `tool`, `size` canvas units wide at full pressure.
    #[uniffi::constructor]
    pub fn new(tool: Tool, size: f32) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(pcore::BrushModeler::new(tool.into(), size)),
        })
    }

    /// A modeler for `brush`: its custom spec's input smoothing when it
    /// has one, the tool's preset otherwise.
    #[uniffi::constructor]
    pub fn for_brush(brush: BrushRef) -> Arc<Self> {
        let tool = brush.tool.into();
        let spec = brush
            .custom
            .as_ref()
            .and_then(CustomBrush::decoded)
            .map_or_else(|| pcore::BrushSpec::preset(tool), |c| c.spec);
        Arc::new(Self {
            inner: Mutex::new(pcore::BrushModeler::for_brush(tool, spec, brush.base_width)),
        })
    }

    /// Feed raw samples in order; the points they produced (fewer than the
    /// samples: near-duplicates are dropped).
    pub fn push(&self, samples: Vec<RawSample>) -> Vec<StrokePoint> {
        let mut inner = self.lock();
        samples
            .into_iter()
            .filter_map(|s| inner.push(s.into()))
            .map(Into::into)
            .collect()
    }

    /// Every point emitted so far.
    pub fn points(&self) -> Vec<StrokePoint> {
        self.lock()
            .points()
            .iter()
            .copied()
            .map(Into::into)
            .collect()
    }

    /// The points `samples` would produce if pushed now, without pushing
    /// them: draw as a tail after `points`, discard next frame.
    pub fn predict(&self, samples: Vec<RawSample>) -> Vec<StrokePoint> {
        let raw: Vec<pcore::RawSample> = samples.into_iter().map(Into::into).collect();
        self.lock()
            .predict(&raw)
            .into_iter()
            .map(Into::into)
            .collect()
    }

    /// The finished stroke's points: pen landed on the last raw sample.
    /// Store these as `PointKind.polylineSample`.
    pub fn finish(&self) -> Vec<StrokePoint> {
        self.lock().finish().into_iter().map(Into::into).collect()
    }

    /// Revise the force and tilt of the point the sample with
    /// `estimation_id` produced (`touchesEstimatedPropertiesUpdated`).
    /// `false` when no emitted point came from that sample.
    pub fn update(&self, estimation_id: u32, force: Option<f32>, tilt: Option<Tilt>) -> bool {
        self.lock()
            .update(estimation_id, force, tilt.map(Into::into))
    }

    /// Estimate ids of emitted points whose revision has not arrived; hold
    /// the commit briefly while this is non-empty.
    pub fn pending_estimates(&self) -> Vec<u32> {
        self.lock().pending_estimates()
    }

    /// The live stroke's ink: every point so far plus the tail `predict`
    /// would add, meshed here so no point crosses the boundary per touch
    /// event. Same geometry as [`points_mesh`] with `StrokeEnd::Live`.
    pub fn live_mesh(
        &self,
        predict: Vec<RawSample>,
        brush: BrushRef,
        color: u32,
        tolerance: f32,
    ) -> InkMesh {
        let raw: Vec<pcore::RawSample> = predict.into_iter().map(Into::into).collect();
        let inner = self.lock();
        let mut points = inner.points().to_vec();
        points.extend(inner.predict(&raw));
        drop(inner);
        brush
            .ink(color)
            .mesh(&points, pcore::StrokeEnd::Live, tolerance)
            .into()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Blend {
    Normal,
    /// Ink multiplies what is under it (highlighter).
    Multiply,
}

/// What happens where a stroke covers itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Overlap {
    /// Both layers show; translucent ink darkens where it crosses itself.
    Accumulate,
    /// Each pixel painted at most once per stroke: draw with the
    /// write-once depth state.
    Discard,
}

/// Everything a renderer applies per stroke rather than per vertex.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct InkStyle {
    /// RGBA8 packed big-endian: 0xRRGGBBAA.
    pub color: u32,
    /// 0..=1, multiplied into the colour's alpha and every vertex opacity.
    pub opacity: f32,
    pub blend: Blend,
    pub overlap: Overlap,
    /// Edge feathering, 1 for a hard edge.
    pub hardness: f32,
    /// What the vertex `uv` means and how the shader cuts the edge.
    pub mask: MaskStyle,
    /// Procedural paper texture the fragment shader multiplies into the
    /// alpha; `None` for flat ink.
    pub grain: Option<GrainStyle>,
}

/// How the fragment shader shapes the ink inside the triangles.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Enum)]
pub enum MaskStyle {
    /// The triangles are the ink; `v` is the side in -1..=1 and a soft tip
    /// feathers the outer band (mask kind 3 in the style flags).
    Ribbon,
    /// Each quad is one dab in tip space `[-1, 1]²`; keep the rounded
    /// superellipse with this corner radius (mask kind 1).
    Shape { corner: f32 },
}

/// What the grain texture is anchored to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum GrainMapping {
    /// Canvas coordinates: every stroke reveals the same paper.
    Canvas,
    /// The stroke's own `(u, v)`, seeded per stroke.
    Stroke,
}

/// Value-noise grain: `mix(1, noise(anchor / scale, seed), strength)`.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct GrainStyle {
    pub mapping: GrainMapping,
    /// Cell size in canvas units.
    pub scale: f32,
    /// 0..=1.
    pub strength: f32,
    /// Hash seed; 0 for canvas-mapped grain.
    pub seed: u32,
}

impl From<pcore::InkStyle> for InkStyle {
    fn from(s: pcore::InkStyle) -> Self {
        Self {
            color: rgba_to_u32(s.color),
            opacity: s.opacity,
            blend: match s.blend {
                pcore::Blend::Normal => Blend::Normal,
                pcore::Blend::Multiply => Blend::Multiply,
            },
            overlap: match s.overlap {
                pcore::Overlap::Accumulate => Overlap::Accumulate,
                pcore::Overlap::Discard => Overlap::Discard,
            },
            hardness: s.hardness,
            mask: match s.mask {
                pcore::MaskStyle::Ribbon => MaskStyle::Ribbon,
                pcore::MaskStyle::Shape { corner } => MaskStyle::Shape { corner },
            },
            grain: s.grain.map(|g| GrainStyle {
                mapping: match g.mapping {
                    pcore::GrainMapping::Canvas => GrainMapping::Canvas,
                    pcore::GrainMapping::Stroke => GrainMapping::Stroke,
                },
                scale: g.scale,
                strength: g.strength,
                seed: g.seed,
            }),
        }
    }
}

/// Floats per vertex in [`InkMesh::vertices`].
pub const INK_VERTEX_FLOATS: u32 = 5;

/// An indexed triangle mesh in canvas units (x right, y down), as the
/// bytes a GPU buffer takes so the boundary is one copy, not a per-element
/// lift. `vertices` is little-endian `f32` `[x, y, u, v, opacity]` per
/// vertex ([`INK_VERTEX_FLOATS`] floats each): `u` is arc length along the
/// stroke, `v` the side in -1..=1, `opacity` 0..=1. `indices` is
/// little-endian `u32`, a triangle list into it. Upload both as-is; apply
/// `style` per draw.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct InkMesh {
    pub vertices: Vec<u8>,
    pub indices: Vec<u8>,
    pub vertex_count: u32,
    pub index_count: u32,
    pub style: InkStyle,
    /// Exact at every zoom (stamped dabs are masked in the shader), so
    /// there is no need to rebuild it when the tolerance changes.
    pub zoom_independent: bool,
}

impl From<pcore::InkMesh> for InkMesh {
    fn from(m: pcore::InkMesh) -> Self {
        Self {
            vertex_count: u32::try_from(m.vertices.len()).unwrap_or(u32::MAX),
            index_count: u32::try_from(m.indices.len()).unwrap_or(u32::MAX),
            zoom_independent: m.zoom_independent,
            vertices: m
                .vertices
                .iter()
                .flat_map(|v| [v.pos[0], v.pos[1], v.uv[0], v.uv[1], v.opacity])
                .flat_map(f32::to_le_bytes)
                .collect(),
            indices: m.indices.iter().flat_map(|i| i.to_le_bytes()).collect(),
            style: m.style.into(),
        }
    }
}

/// Floats per vertex in [`InkMesh::vertices`].
#[uniffi::export]
pub fn ink_vertex_floats() -> u32 {
    INK_VERTEX_FLOATS
}

/// Cap/join flattening tolerance for 1:1 zoom, canvas units.
#[uniffi::export]
pub fn default_tolerance() -> f32 {
    pcore::DEFAULT_TOLERANCE
}

/// The committed stroke's ink as an indexed mesh. `tolerance` bounds
/// cap/join flattening error in canvas units; pass [`default_tolerance`]
/// divided by the zoom factor.
#[uniffi::export]
pub fn stroke_mesh(stroke: Stroke, tolerance: f32) -> InkMesh {
    pcore::Stroke::from(stroke).mesh(tolerance).into()
}

/// Ink for a run of stored points: the live stroke (`BrushModeler.points`
/// plus its `predict` tail, `StrokeEnd.live`) or a remote wet run
/// received on the ephemeral channel (`StrokeEnd.live` until its `End`).
/// Same geometry as [`stroke_mesh`] for the same points, so the committed
/// stroke lands on top without a visible change.
#[uniffi::export]
pub fn points_mesh(
    points: Vec<StrokePoint>,
    brush: BrushRef,
    color: u32,
    end: StrokeEnd,
    tolerance: f32,
) -> InkMesh {
    let flat: Vec<pcore::StrokePoint> = points.into_iter().map(Into::into).collect();
    brush.ink(color).mesh(&flat, end.into(), tolerance).into()
}

/// Ink for a committed element, stroke or shape, same geometry as
/// [`stroke_mesh`]; a shape's outline is built here so it never crosses
/// the FFI.
#[uniffi::export]
pub fn element_mesh(element: Element, tolerance: f32) -> InkMesh {
    let element = pcore::Element::from(element);
    element
        .ink()
        .mesh(&element.outline(), pcore::StrokeEnd::Complete, tolerance)
        .into()
}

/// Ink for a shape that is not committed yet (the hold preview), drawn
/// with the live stroke's brush and colour.
#[uniffi::export]
pub fn shape_outline_mesh(shape: Shape, brush: BrushRef, color: u32, tolerance: f32) -> InkMesh {
    let outline = pcore::Shape::from(shape).outline();
    brush
        .ink(color)
        .mesh(&outline, pcore::StrokeEnd::Complete, tolerance)
        .into()
}

/// The mark the tip would leave touching down at (`x`, `y`) with the pen
/// held at `tilt` and average pressure: the Pencil Pro hover preview,
/// drawn by the renderer at reduced alpha. Same ink as a one-point stroke.
#[uniffi::export]
pub fn hover_dab_mesh(
    brush: BrushRef,
    color: u32,
    x: f32,
    y: f32,
    tilt: Option<Tilt>,
    tolerance: f32,
) -> InkMesh {
    brush
        .ink(color)
        .hover_dab(x, y, tilt.map(Into::into), tolerance)
        .into()
}

/// Snap a live stroke (`BrushModeler.points` at hold time) to a line,
/// arrow, rectangle or ellipse; `None` when it is not drawn cleanly enough
/// to be one. `hold_radius` is how far, in canvas units, the pen may have
/// wandered during the hold (the caller's own stillness threshold plus
/// slack); the tail inside it is collapsed before recognition.
#[uniffi::export]
pub fn recognize_shape(points: Vec<StrokePoint>, hold_radius: f32) -> Option<Recognition> {
    let pts: Vec<pcore::StrokePoint> = points.into_iter().map(Into::into).collect();
    let params = pcore::RecognizerParams {
        hold_radius: hold_radius.max(0.0),
        ..pcore::RecognizerParams::default()
    };
    pcore::recognize_with(&pts, &params).map(Into::into)
}

/// The snapped shape after the pen, held at `from` when it snapped, is
/// dragged to `to`: a line or arrow moves the endpoint under the pen, a
/// rectangle drags the side or corner under the pen with the opposite side
/// fixed, an ellipse scales about the point opposite the pen. Pass the
/// original snapped shape and `from` on every move.
#[uniffi::export]
pub fn resize_shape(shape: Shape, from: Point2, to: Point2) -> Shape {
    pcore::Shape::from(shape)
        .resized(from.into(), to.into())
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PointKind;

    fn samples(n: usize) -> Vec<RawSample> {
        (0..n)
            .map(|i| RawSample {
                x: i as f32 * 3.0,
                y: i as f32,
                force: 0.7,
                t_ms: 100.0 + i as f64 * 8.0,
                tilt: None,
                estimation_id: None,
                expects_update: false,
            })
            .collect()
    }

    #[test]
    fn modeler_round_trips_to_a_mesh() {
        let m = BrushModeler::new(Tool::Pen, 4.0);
        let live = m.push(samples(16));
        assert!(!live.is_empty());
        assert_eq!(m.points(), live);
        let tail = m.predict(vec![RawSample {
            x: 60.0,
            y: 20.0,
            force: 0.7,
            t_ms: 300.0,
            tilt: None,
            estimation_id: None,
            expects_update: false,
        }]);
        assert_eq!(tail.len(), 1);
        assert_eq!(m.points(), live, "predict must not push");

        let brush = BrushRef {
            tool: Tool::Pen,
            base_width: 4.0,
            seed: 0,
            custom: None,
        };
        assert_eq!(
            m.live_mesh(vec![], brush.clone(), 0xff, 0.25),
            points_mesh(m.points(), brush.clone(), 0xff, StrokeEnd::Live, 0.25),
            "live_mesh is points_mesh of the modeler's points"
        );
        let done = m.finish();
        let mesh = points_mesh(done.clone(), brush.clone(), 0xff, StrokeEnd::Complete, 0.25);
        assert!(mesh.index_count >= 3 && mesh.index_count.is_multiple_of(3));
        assert_eq!(
            mesh.indices.len(),
            usize::try_from(mesh.index_count).unwrap() * 4
        );
        let stride = usize::try_from(INK_VERTEX_FLOATS).unwrap() * 4;
        assert_eq!(
            mesh.vertices.len(),
            usize::try_from(mesh.vertex_count).unwrap() * stride
        );
        let indices: Vec<u32> = mesh
            .indices
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        assert!(indices.iter().all(|&i| i < mesh.vertex_count));
        assert_eq!(mesh.style.color, 0xff);

        let committed = stroke_mesh(
            Stroke {
                id: pcore::StrokeId::new().to_string(),
                tool: Tool::Pen,
                color: 0xff,
                base_width: 4.0,
                kind: PointKind::PolylineSample,
                points: done.clone(),
                created_ms: 0,
                brush: None,
            },
            0.25,
        );
        assert_eq!(committed, mesh, "live and committed ink are the same mesh");
        let live_end = points_mesh(done, brush, 0xff, StrokeEnd::Live, 0.25);
        assert_ne!(live_end, mesh, "only the finished stroke tapers");
    }

    #[test]
    fn a_held_rectangle_snaps_and_meshes() {
        let corners = [
            [0.0, 0.0],
            [120.0, 1.0],
            [119.0, 80.0],
            [1.0, 79.0],
            [0.0, 2.0],
        ];
        let raw: Vec<RawSample> = corners
            .windows(2)
            .flat_map(|w| {
                (0..30).map(move |i| {
                    let t = i as f32 / 30.0;
                    [
                        w[0][0] + (w[1][0] - w[0][0]) * t,
                        w[0][1] + (w[1][1] - w[0][1]) * t,
                    ]
                })
            })
            .enumerate()
            .map(|(i, [x, y])| RawSample {
                x,
                y,
                force: 0.6,
                t_ms: i as f64 * 8.0,
                tilt: None,
                estimation_id: None,
                expects_update: false,
            })
            .collect();
        let m = BrushModeler::new(Tool::Pen, 4.0);
        m.push(raw);
        let rec = recognize_shape(m.points(), 4.5).expect("rectangle");
        let Shape::Rect { size, .. } = rec.shape else {
            panic!("expected a rect, got {rec:?}");
        };
        assert!((size.x - 120.0).abs() < 6.0 && (size.y - 80.0).abs() < 6.0);
        assert!(rec.confidence > 0.0);

        let brush = BrushRef {
            tool: Tool::Pen,
            base_width: 4.0,
            seed: 0,
            custom: None,
        };
        let preview = shape_outline_mesh(rec.shape, brush, 0xff, 0.25);
        let committed = element_mesh(
            Element::Shape(crate::types::ShapeElement {
                id: pcore::ElementId::new().to_string(),
                shape: rec.shape,
                tool: Tool::Pen,
                color: 0xff,
                width: 4.0,
                start: None,
                end: None,
                created_ms: 0,
            }),
            0.25,
        );
        assert_eq!(
            preview, committed,
            "preview and committed ink are the same mesh"
        );
        assert!(committed.index_count > 0);
    }
}
