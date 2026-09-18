//! The mesh every renderer draws: triangles in canvas space with the
//! per-vertex attributes a brush shader needs, plus the per-stroke style.

use crate::brush::{Blend, Overlap, Tip};
use crate::stroke::Rgba;

/// One mesh vertex.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InkVertex {
    /// Canvas position (x right, y down).
    pub pos: [f32; 2],
    /// Stroke-space coordinate: `u` is arc length along the stroke in
    /// canvas units, `v` is the side, -1 on the left edge through +1 on
    /// the right. Renderers map grain and masks through it.
    pub uv: [f32; 2],
    /// 0..=1, multiplied into the stroke colour's alpha.
    pub opacity: f32,
}

/// Everything a renderer applies per stroke rather than per vertex.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InkStyle {
    pub color: Rgba,
    /// 0..=1, multiplied into `color`'s alpha and every vertex opacity.
    pub opacity: f32,
    pub blend: Blend,
    pub overlap: Overlap,
    /// Edge feathering, 1 for a hard edge (see [`Tip::hardness`]).
    pub hardness: f32,
}

impl InkStyle {
    /// Opaque black, hard, accumulating: the style of a plain pen.
    pub const PLAIN: Self = Self {
        color: Rgba::BLACK,
        opacity: 1.0,
        blend: Blend::Normal,
        overlap: Overlap::Accumulate,
        hardness: 1.0,
    };

    pub(crate) fn of(color: Rgba, tip: &Tip, paint: &crate::brush::Paint) -> Self {
        Self {
            color,
            opacity: paint.opacity.clamp(0.0, 1.0),
            blend: paint.blend,
            overlap: paint.overlap,
            hardness: tip.hardness.clamp(0.0, 1.0),
        }
    }
}

/// A stroke tessellated into triangles, in canvas space (x right, y down).
/// Renderers flip y as their convention requires.
#[derive(Debug, Clone, PartialEq)]
pub struct InkMesh {
    pub vertices: Vec<InkVertex>,
    /// Triangle list into `vertices`.
    pub indices: Vec<u32>,
    pub style: InkStyle,
}

impl Default for InkMesh {
    fn default() -> Self {
        Self::empty(InkStyle::PLAIN)
    }
}

impl InkMesh {
    pub fn empty(style: InkStyle) -> Self {
        Self {
            vertices: Vec::new(),
            indices: Vec::new(),
            style,
        }
    }

    /// Nothing to draw.
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    /// Vertex positions in order.
    pub fn positions(&self) -> impl Iterator<Item = [f32; 2]> + '_ {
        self.vertices.iter().map(|v| v.pos)
    }

    /// Axis-aligned bounds as `(min, max)`, or `None` when empty.
    pub fn bounds(&self) -> Option<([f32; 2], [f32; 2])> {
        self.positions().fold(None, |acc, [x, y]| {
            let (lo, hi) = acc.unwrap_or(([x, y], [x, y]));
            Some(([lo[0].min(x), lo[1].min(y)], [hi[0].max(x), hi[1].max(y)]))
        })
    }
}
