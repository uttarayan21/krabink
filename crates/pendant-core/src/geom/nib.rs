//! Oriented tips: the ink is a ribbon whose edges are the two ends of a
//! nib turned to the tip rotation rather than the direction of motion,
//! which is what makes calligraphic strokes thick across the nib and thin
//! along it. Each point also stamps the nib's own rectangle, so a stroke
//! starts and ends with the nib's flat edge and a chisel that turns keeps
//! its corners.

use super::{InkMesh, InkStyle, InkVertex};
use crate::brush::{MIN_TIP, TipState};

/// Thinnest the ribbon gets across the path, as a fraction of the nib
/// length, when the nib is dragged along its own length.
pub(super) const NIB_MIN_FRACTION: f32 = 0.08;

/// Ribbon plus per-point nib rectangles. `w` is the nib's length, `h` its
/// thickness, `rot` where it points.
pub(super) fn nib_mesh(pts: &[([f32; 2], TipState)], style: InkStyle) -> InkMesh {
    let mut mesh = InkMesh::empty(style);
    let Some(&(first, first_tip)) = pts.first() else {
        return mesh;
    };
    if pts.len() == 1 {
        push_rect(&mut mesh, first, &first_tip, 0.0);
        return mesh;
    }

    // The ribbon: two edge vertices per point, then a strip through them.
    let mut arc = 0.0;
    for (i, &(p, tip)) in pts.iter().enumerate() {
        let prev = pts[i.saturating_sub(1)].0;
        let next = pts[(i + 1).min(pts.len() - 1)].0;
        if i > 0 {
            arc += (p[0] - prev[0]).hypot(p[1] - prev[1]);
        }
        let (dx, dy) = (next[0] - prev[0], next[1] - prev[1]);
        let len = (dx * dx + dy * dy).sqrt().max(f32::EPSILON);
        let (nx, ny) = (-dy / len, dx / len);
        let half = (tip.w / 2.0).max(MIN_TIP);
        let floor = (tip.w * NIB_MIN_FRACTION / 2.0).max(MIN_TIP);
        let (ux, uy) = (tip.rot.cos(), tip.rot.sin());
        // Keep the nib's side that faces the motion normal first so the
        // strip never twists when the nib crosses the path.
        let side = if ux * nx + uy * ny < 0.0 { -1.0 } else { 1.0 };
        // Guarantee a minimum thickness across the path.
        let across = (ux * nx + uy * ny).abs() * half;
        let (ox, oy) = if across < floor {
            (nx * floor, ny * floor)
        } else {
            (ux * half * side, uy * half * side)
        };
        let vertex = |pos: [f32; 2], v: f32| InkVertex {
            pos,
            uv: [arc, v],
            opacity: tip.opacity,
        };
        mesh.vertices.push(vertex([p[0] + ox, p[1] + oy], 1.0));
        mesh.vertices.push(vertex([p[0] - ox, p[1] - oy], -1.0));
    }
    let pairs = u32::try_from(pts.len() - 1).unwrap_or(u32::MAX);
    mesh.indices.extend((0..pairs).flat_map(|i| {
        let base = i * 2;
        [base, base + 1, base + 2, base + 2, base + 1, base + 3]
    }));

    // The nib itself at every point: the caps, and the corners a turning
    // chisel would otherwise lose.
    let mut arc = 0.0;
    for (i, &(p, tip)) in pts.iter().enumerate() {
        if i > 0 {
            let prev = pts[i - 1].0;
            arc += (p[0] - prev[0]).hypot(p[1] - prev[1]);
        }
        push_rect(&mut mesh, p, &tip, arc);
    }
    mesh
}

/// The nib's rectangle at `p`: `w` long along `rot`, `h` thick across it.
fn push_rect(mesh: &mut InkMesh, p: [f32; 2], tip: &TipState, arc: f32) {
    let (ux, uy) = (tip.rot.cos(), tip.rot.sin());
    let (half_len, half_thick) = ((tip.w / 2.0).max(MIN_TIP), (tip.h / 2.0).max(MIN_TIP));
    let (lx, ly) = (ux * half_len, uy * half_len);
    let (tx, ty) = (-uy * half_thick, ux * half_thick);
    let base = u32::try_from(mesh.vertices.len()).unwrap_or(u32::MAX);
    let vertex = |pos: [f32; 2], v: f32| InkVertex {
        pos,
        uv: [arc, v],
        opacity: tip.opacity,
    };
    mesh.vertices.extend([
        vertex([p[0] + lx + tx, p[1] + ly + ty], 1.0),
        vertex([p[0] - lx + tx, p[1] - ly + ty], -1.0),
        vertex([p[0] + lx - tx, p[1] + ly - ty], 1.0),
        vertex([p[0] - lx - tx, p[1] - ly - ty], -1.0),
    ]);
    mesh.indices
        .extend([base, base + 1, base + 2, base + 2, base + 1, base + 3]);
}
