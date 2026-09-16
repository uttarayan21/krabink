//! Round-tipped strokes through lyon's stroker: a variable-width path with
//! round caps and round joins, one width attribute per point.

use lyon_tessellation::{
    BuffersBuilder, LineCap, LineJoin, StrokeOptions, StrokeTessellator, StrokeVertex,
    VertexBuffers, math::point, path::Path,
};

use super::{InkMesh, InkVertex, MIN_SEGMENT, dedupe, half_width};
use crate::stroke::StrokePoint;

/// Variable-width stroke through lyon: width per point is the point's own
/// `size.w` when present, otherwise `base_width * force`; caps and joins
/// are round. A single point (or all-coincident points) becomes a dot of
/// the point's width.
pub(super) fn round_mesh(points: &[StrokePoint], base_width: f32, tolerance: f32) -> InkMesh {
    let pts = dedupe(points);
    let Some(first) = pts.first() else {
        return InkMesh::default();
    };
    let width = |p: &StrokePoint| 2.0 * half_width(p, base_width);

    let mut builder = Path::builder_with_attributes(1);
    builder.begin(point(first.x, first.y), &[width(first)]);
    if pts.len() == 1 {
        // A zero-length segment is skipped by the stroker; nudge the end so
        // the two round caps meet as a circle.
        builder.line_to(point(first.x + MIN_SEGMENT, first.y), &[width(first)]);
    }
    for p in pts.iter().skip(1) {
        builder.line_to(point(p.x, p.y), &[width(p)]);
    }
    builder.end(false);
    let path = builder.build();

    // Lyon drops geometry finer than its tolerance, so a coarse tolerance
    // on hairline ink would erase the stroke: never exceed a quarter of the
    // thinnest width in play.
    let finest = pts.iter().map(width).fold(f32::INFINITY, f32::min);
    let tolerance = tolerance.clamp(1e-3, (finest / 4.0).max(1e-3));
    let options = StrokeOptions::tolerance(tolerance)
        .with_line_width(1.0)
        .with_variable_line_width(0)
        .with_line_cap(LineCap::Round)
        .with_line_join(LineJoin::Round);
    let mut buffers: VertexBuffers<InkVertex, u32> = VertexBuffers::new();
    let result = StrokeTessellator::new().tessellate_path(
        &path,
        &options,
        &mut BuffersBuilder::new(&mut buffers, |v: StrokeVertex| {
            let p = v.position();
            InkVertex {
                pos: [p.x, p.y],
                uv: [v.advancement(), v.side().to_f32()],
                opacity: 1.0,
            }
        }),
    );
    if let Err(err) = result {
        tracing::warn!(?err, points = pts.len(), "stroke tessellation failed");
        return InkMesh::default();
    }
    InkMesh {
        vertices: buffers.vertices,
        indices: buffers.indices,
    }
}
