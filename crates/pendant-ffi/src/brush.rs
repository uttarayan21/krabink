//! Brush model and mesh surface for the native ink renderer: raw touches in,
//! modelled points and indexed triangle meshes out. Everything above
//! "upload vertices, draw triangles" lives in `pendant-core` so the iPad and
//! the desktop draw the same ink (`docs/plans/ink-renderer.md`).

use std::sync::{Arc, Mutex, PoisonError};

use pendant_core as pcore;

use crate::types::{Stroke, StrokePoint, Tilt, Tool, WetPoint};

/// One raw touch sample, before smoothing.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct RawSample {
    pub x: f32,
    pub y: f32,
    /// Normalised pressure, 0..=1. Touches without pressure pass 1.
    pub force: f32,
    /// Milliseconds on any monotonic clock (`UITouch.timestamp * 1000`);
    /// only differences matter.
    pub t_ms: f64,
    pub tilt: Option<Tilt>,
}

impl From<RawSample> for pcore::RawSample {
    fn from(s: RawSample) -> Self {
        Self {
            x: s.x,
            y: s.y,
            force: s.force,
            t_ms: s.t_ms,
            tilt: s.tilt.map(Into::into),
        }
    }
}

/// One live stroke's input pipeline: streamline smoothing, speed and force
/// width model, tapers. Create at pen-down, `push` every coalesced touch,
/// draw `points` plus a `predict` tail each frame, commit `finish` at
/// pen-up. Thread-safe; the touch handler and the render loop may share it.
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

    /// Every point emitted so far, without the end taper.
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

    /// The finished stroke's points: pen landed on the last raw sample,
    /// tail tapered. Store these as `PointKind.polylineSample`.
    pub fn finish(&self) -> Vec<StrokePoint> {
        self.lock().finish().into_iter().map(Into::into).collect()
    }
}

/// The wet-ink view of modelled points: rendered width and nib angle, so
/// receivers draw exactly what the sender drew.
#[uniffi::export]
pub fn wet_points(points: Vec<StrokePoint>) -> Vec<WetPoint> {
    points
        .into_iter()
        .map(|p| pcore::WetPoint::from(pcore::StrokePoint::from(p)).into())
        .collect()
}

/// An indexed triangle mesh in canvas units (x right, y down).
/// `positions` is flat `[x0, y0, x1, y1, …]`; `indices` is a triangle list
/// into it. Upload both as-is to a vertex and index buffer.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct IndexedMesh {
    pub positions: Vec<f32>,
    pub indices: Vec<u32>,
}

impl From<pcore::StrokeMesh> for IndexedMesh {
    fn from(m: pcore::StrokeMesh) -> Self {
        Self {
            positions: m.positions.into_iter().flatten().collect(),
            indices: m.indices,
        }
    }
}

/// The committed stroke's ink as an indexed mesh. `tolerance` bounds
/// cap/join flattening error in canvas units; pass
/// [`default_tolerance`](crate::types::default_tolerance) divided by the
/// zoom factor.
#[uniffi::export]
pub fn stroke_mesh(stroke: Stroke, tolerance: f32) -> IndexedMesh {
    let stroke = pcore::Stroke::from(stroke);
    let flat = stroke.flatten();
    pcore::stroke_mesh(stroke.tool, &flat, stroke.base_width, tolerance).into()
}

/// Ink for a live run of modelled points (`BrushModeler.points` plus its
/// `predict` tail), same geometry as [`stroke_mesh`].
#[uniffi::export]
pub fn points_mesh(
    points: Vec<StrokePoint>,
    tool: Tool,
    base_width: f32,
    tolerance: f32,
) -> IndexedMesh {
    let flat: Vec<pcore::StrokePoint> = points.into_iter().map(Into::into).collect();
    pcore::stroke_mesh(tool.into(), &flat, base_width, tolerance).into()
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
        }]);
        assert_eq!(tail.len(), 1);
        assert_eq!(m.points(), live, "predict must not push");

        let done = m.finish();
        let wet = wet_points(done.clone());
        assert_eq!(wet.len(), done.len());
        assert!(wet.iter().all(|w| w.width.is_some()));

        let mesh = points_mesh(done.clone(), Tool::Pen, 4.0, 0.25);
        assert!(mesh.indices.len() >= 3 && mesh.indices.len().is_multiple_of(3));
        assert!(mesh.positions.len().is_multiple_of(2));
        let vertices = u32::try_from(mesh.positions.len() / 2).unwrap();
        assert!(mesh.indices.iter().all(|&i| i < vertices));

        let committed = stroke_mesh(
            Stroke {
                id: pcore::StrokeId::new().to_string(),
                tool: Tool::Pen,
                color: 0xff,
                base_width: 4.0,
                kind: PointKind::PolylineSample,
                points: done,
                created_ms: 0,
            },
            0.25,
        );
        assert_eq!(committed, mesh, "live and committed ink are the same mesh");
    }
}
