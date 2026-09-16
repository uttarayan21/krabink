//! Flat-nib strokes: the ink is a ribbon whose two edges are the ends of
//! the nib, turned to the pen's orientation rather than the direction of
//! motion, which is what makes calligraphic strokes thick across the nib
//! and thin along it.

use super::{InkMesh, InkVertex, MIN_HALF_WIDTH, dedupe};
use crate::stroke::StrokePoint;

/// Thinnest a flat nib gets when dragged along its own length, as a
/// fraction of `base_width`.
pub(super) const NIB_MIN_FRACTION: f32 = 0.08;

/// Flat-nib tessellation: each point becomes the two ends of a `base_width`
/// long nib turned to `tilt.nib_angle()` (azimuth + barrel roll). Points
/// without orientation fall back to the motion normal, so a stroke from a
/// pen that cannot report roll still renders.
pub fn nib_ribbon(points: &[StrokePoint], base_width: f32) -> InkMesh {
    let pts = dedupe(points);
    let half = (base_width / 2.0).max(MIN_HALF_WIDTH);
    let floor = (base_width * NIB_MIN_FRACTION / 2.0).max(MIN_HALF_WIDTH);
    let vertex = |pos: [f32; 2], uv: [f32; 2]| InkVertex {
        pos,
        uv,
        opacity: 1.0,
    };
    match pts.as_slice() {
        [] => return InkMesh::default(),
        [p] => {
            // A nib touched down without moving: a square dot of its width.
            let h = half;
            return InkMesh {
                vertices: vec![
                    vertex([p.x - h, p.y - h], [0.0, -1.0]),
                    vertex([p.x + h, p.y - h], [0.0, 1.0]),
                    vertex([p.x - h, p.y + h], [2.0 * h, -1.0]),
                    vertex([p.x + h, p.y + h], [2.0 * h, 1.0]),
                ],
                indices: vec![0, 1, 2, 2, 1, 3],
            };
        }
        _ => {}
    }
    let mut vertices = Vec::with_capacity(pts.len() * 2);
    let mut arc = 0.0;
    for (i, p) in pts.iter().enumerate() {
        let prev = &pts[i.saturating_sub(1)];
        let next = &pts[(i + 1).min(pts.len() - 1)];
        if i > 0 {
            arc += (p.x - prev.x).hypot(p.y - prev.y);
        }
        let (dx, dy) = (next.x - prev.x, next.y - prev.y);
        let len = (dx * dx + dy * dy).sqrt().max(f32::EPSILON);
        let (nx, ny) = (-dy / len, dx / len);
        let (ox, oy) = match p.tilt {
            Some(tilt) => {
                let angle = tilt.nib_angle();
                let (ux, uy) = (angle.cos(), angle.sin());
                // Keep the nib's side that faces the motion normal first so
                // the strip never twists when the nib crosses the path.
                let side = if ux * nx + uy * ny < 0.0 { -1.0 } else { 1.0 };
                // Guarantee a minimum thickness across the path.
                let across = (ux * nx + uy * ny).abs() * half;
                if across < floor {
                    (nx * floor, ny * floor)
                } else {
                    (ux * half * side, uy * half * side)
                }
            }
            None => (nx * half, ny * half),
        };
        vertices.push(vertex([p.x + ox, p.y + oy], [arc, 1.0]));
        vertices.push(vertex([p.x - ox, p.y - oy], [arc, -1.0]));
    }
    InkMesh {
        indices: strip_indices(pts.len()),
        vertices,
    }
}

/// Triangle-list indices for a strip of `points` (left, right) vertex pairs.
fn strip_indices(points: usize) -> Vec<u32> {
    let pairs = u32::try_from(points.saturating_sub(1)).unwrap_or(u32::MAX);
    (0..pairs)
        .flat_map(|i| {
            let base = i * 2;
            [base, base + 1, base + 2, base + 2, base + 1, base + 3]
        })
        .collect()
}
