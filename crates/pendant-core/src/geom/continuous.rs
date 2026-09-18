//! Round-tipped strokes through lyon's stroker: a variable-width path with
//! round caps and round joins, width and opacity attributes per point.

use lyon_tessellation::{
    BuffersBuilder, LineCap, LineJoin, StrokeOptions, StrokeTessellator, StrokeVertex,
    VertexBuffers, math::point, path::Path,
};

use super::{InkMesh, InkStyle, InkVertex, MIN_SEGMENT};
use crate::brush::TipState;

/// Variable-width stroke through lyon with round caps and joins. A single
/// point (or all-coincident points) becomes a dot of the point's width.
pub(super) fn round_mesh(pts: &[([f32; 2], TipState)], style: InkStyle, tolerance: f32) -> InkMesh {
    let Some((first, first_tip)) = pts.first() else {
        return InkMesh::empty(style);
    };
    let attrs = |t: &TipState| [t.w, t.opacity];

    let mut builder = Path::builder_with_attributes(2);
    builder.begin(point(first[0], first[1]), &attrs(first_tip));
    if pts.len() == 1 {
        // A zero-length segment is skipped by the stroker; nudge the end so
        // the two round caps meet as a circle.
        builder.line_to(point(first[0] + MIN_SEGMENT, first[1]), &attrs(first_tip));
    }
    for (p, tip) in pts.iter().skip(1) {
        builder.line_to(point(p[0], p[1]), &attrs(tip));
    }
    builder.end(false);
    let path = builder.build();

    // Lyon drops geometry finer than its tolerance, so a coarse tolerance
    // on hairline ink would erase the stroke: never exceed a quarter of the
    // thinnest width in play.
    let finest = pts.iter().map(|(_, t)| t.w).fold(f32::INFINITY, f32::min);
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
        &mut BuffersBuilder::new(&mut buffers, |mut v: StrokeVertex| {
            let p = v.position();
            let opacity = v.interpolated_attributes()[1];
            InkVertex {
                pos: [p.x, p.y],
                uv: [v.advancement(), v.side().to_f32()],
                opacity: opacity.clamp(0.0, 1.0),
            }
        }),
    );
    if let Err(err) = result {
        tracing::warn!(?err, points = pts.len(), "stroke tessellation failed");
        return InkMesh::empty(style);
    }
    InkMesh {
        vertices: buffers.vertices,
        indices: buffers.indices,
        style,
    }
}
