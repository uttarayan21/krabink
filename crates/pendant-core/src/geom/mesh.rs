//! The mesh every renderer draws: triangles in canvas space with the
//! per-vertex attributes a brush shader needs.

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

/// A stroke tessellated into triangles, in canvas space (x right, y down).
/// Renderers flip y as their convention requires.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InkMesh {
    pub vertices: Vec<InkVertex>,
    /// Triangle list into `vertices`.
    pub indices: Vec<u32>,
}

impl InkMesh {
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
