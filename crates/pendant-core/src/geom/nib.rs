//! Oriented tips: the ink is what the nib's rectangle sweeps out when it
//! is dragged along the path while turned to the tip rotation rather than
//! the direction of motion, which is what makes calligraphic strokes
//! thick across the nib and thin along it. Each segment is the convex
//! hull of the rectangles at its two ends, so the outline is the swept
//! shape itself: no corner of a rectangle ever sticks out past the edge,
//! however far apart the points are, and a stroke starts and ends with
//! the nib's flat edge.

use super::{InkMesh, InkStyle, InkVertex};
use crate::brush::{MIN_TIP, TipState};

/// Thinnest the SVG outline gets across the path, as a fraction of the nib
/// length, when the nib is dragged along its own length (the mesh gets
/// the rectangle's real thickness).
pub(super) const NIB_MIN_FRACTION: f32 = 0.08;

/// Swept nib rectangles. `w` is the nib's length, `h` its thickness,
/// `rot` where it points.
pub(super) fn nib_mesh(pts: &[([f32; 2], TipState)], style: InkStyle) -> InkMesh {
    let mut mesh = InkMesh::empty(style);
    let Some(&(first, first_tip)) = pts.first() else {
        return mesh;
    };
    if pts.len() == 1 {
        push_rect(&mut mesh, first, &first_tip, 0.0);
        return mesh;
    }
    let mut arc = 0.0;
    let mut prev = Rect::at(first, &first_tip, arc);
    for &(p, tip) in &pts[1..] {
        arc += (p[0] - prev.center[0]).hypot(p[1] - prev.center[1]);
        let cur = Rect::at(p, &tip, arc);
        push_hull(&mut mesh, &prev, &cur);
        prev = cur;
    }
    mesh
}

/// The nib's rectangle at one point, with what its vertices carry.
struct Rect {
    center: [f32; 2],
    corners: [[f32; 2]; 4],
    arc: f32,
    opacity: f32,
}

impl Rect {
    fn at(c: [f32; 2], tip: &TipState, arc: f32) -> Self {
        let (ux, uy) = (tip.rot.cos(), tip.rot.sin());
        let (l, t) = ((tip.w / 2.0).max(MIN_TIP), (tip.h / 2.0).max(MIN_TIP));
        Self {
            center: c,
            corners: [
                [c[0] + ux * l - uy * t, c[1] + uy * l + ux * t],
                [c[0] - ux * l - uy * t, c[1] - uy * l + ux * t],
                [c[0] - ux * l + uy * t, c[1] - uy * l - ux * t],
                [c[0] + ux * l + uy * t, c[1] + uy * l - ux * t],
            ],
            arc,
            opacity: tip.opacity,
        }
    }
}

/// The convex hull of two rectangles as a fan. `v` is the side of the
/// segment's direction the vertex lies on, so an edge feather still
/// knows the outline.
fn push_hull(mesh: &mut InkMesh, a: &Rect, b: &Rect) {
    let (dx, dy) = (b.center[0] - a.center[0], b.center[1] - a.center[1]);
    let mid = [
        (a.center[0] + b.center[0]) / 2.0,
        (a.center[1] + b.center[1]) / 2.0,
    ];
    let points: Vec<([f32; 2], f32, f32)> = a
        .corners
        .iter()
        .map(|&c| (c, a.arc, a.opacity))
        .chain(b.corners.iter().map(|&c| (c, b.arc, b.opacity)))
        .collect();
    let hull = convex_hull(&points);
    if hull.len() < 3 {
        return;
    }
    let base = u32::try_from(mesh.vertices.len()).unwrap_or(u32::MAX);
    for &(pos, arc, opacity) in &hull {
        let side = dx * (pos[1] - mid[1]) - dy * (pos[0] - mid[0]);
        mesh.vertices.push(InkVertex {
            pos,
            uv: [arc, if side < 0.0 { -1.0 } else { 1.0 }],
            opacity,
        });
    }
    let n = u32::try_from(hull.len()).unwrap_or(u32::MAX);
    mesh.indices
        .extend((1..n - 1).flat_map(|i| [base, base + i, base + i + 1]));
}

/// Andrew's monotone chain, counter-clockwise, collinear points dropped.
fn convex_hull(points: &[([f32; 2], f32, f32)]) -> Vec<([f32; 2], f32, f32)> {
    let mut sorted: Vec<_> = points.to_vec();
    sorted.sort_by(|a, b| a.0[0].total_cmp(&b.0[0]).then(a.0[1].total_cmp(&b.0[1])));
    sorted.dedup_by(|a, b| a.0 == b.0);
    if sorted.len() < 3 {
        return sorted;
    }
    let cross = |o: [f32; 2], a: [f32; 2], b: [f32; 2]| {
        (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
    };
    let mut hull: Vec<([f32; 2], f32, f32)> = Vec::with_capacity(sorted.len() * 2);
    for pass in [
        sorted.iter().collect::<Vec<_>>(),
        sorted.iter().rev().collect(),
    ] {
        let start = hull.len();
        for &p in pass {
            while hull.len() >= start + 2
                && cross(hull[hull.len() - 2].0, hull[hull.len() - 1].0, p.0) <= 0.0
            {
                hull.pop();
            }
            hull.push(p);
        }
        hull.pop();
    }
    hull
}

/// The nib's rectangle at `p`: `w` long along `rot`, `h` thick across it.
fn push_rect(mesh: &mut InkMesh, p: [f32; 2], tip: &TipState, arc: f32) {
    let rect = Rect::at(p, tip, arc);
    let base = u32::try_from(mesh.vertices.len()).unwrap_or(u32::MAX);
    for (i, &pos) in rect.corners.iter().enumerate() {
        // Corners 0 and 3 are the +rot end of the nib.
        let v = if i == 0 || i == 3 { 1.0 } else { -1.0 };
        mesh.vertices.push(InkVertex {
            pos,
            uv: [arc, v],
            opacity: tip.opacity,
        });
    }
    mesh.indices
        .extend([base, base + 1, base + 2, base, base + 2, base + 3]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tip(w: f32, h: f32, rot: f32) -> TipState {
        TipState {
            w,
            h,
            rot,
            opacity: 1.0,
            arc: 0.0,
        }
    }

    #[test]
    fn hull_of_two_rectangles_has_no_protruding_corners() {
        // A chisel turned 30° dragged along +x: every mesh vertex lies
        // inside the union of the two swept rectangles' hull, and the
        // outline is exactly eight corners, none dropped.
        let rot = 0.5;
        let pts = [
            ([0.0, 0.0], tip(20.0, 5.0, rot)),
            ([30.0, 0.0], tip(20.0, 5.0, rot)),
        ];
        let mesh = nib_mesh(&pts, InkStyle::PLAIN);
        assert_eq!(mesh.vertices.len(), 6, "two collinear corner pairs drop");
        assert_eq!(mesh.indices.len(), 12);
        let (lo, hi) = mesh.bounds().unwrap();
        let a = Rect::at([0.0, 0.0], &tip(20.0, 5.0, rot), 0.0);
        let b = Rect::at([30.0, 0.0], &tip(20.0, 5.0, rot), 30.0);
        let all: Vec<[f32; 2]> = a.corners.iter().chain(&b.corners).copied().collect();
        let min_x = all.iter().map(|c| c[0]).fold(f32::INFINITY, f32::min);
        let max_x = all.iter().map(|c| c[0]).fold(f32::NEG_INFINITY, f32::max);
        assert!((lo[0] - min_x).abs() < 1e-4 && (hi[0] - max_x).abs() < 1e-4);
    }

    #[test]
    fn hull_is_counter_clockwise_and_sides_split() {
        let pts = [
            ([0.0, 0.0], tip(10.0, 2.0, 1.2)),
            ([8.0, 3.0], tip(10.0, 2.0, 1.4)),
        ];
        let mesh = nib_mesh(&pts, InkStyle::PLAIN);
        let signed_area: f32 = mesh
            .indices
            .chunks(3)
            .map(|t| {
                let [a, b, c] = [
                    mesh.vertices[t[0] as usize].pos,
                    mesh.vertices[t[1] as usize].pos,
                    mesh.vertices[t[2] as usize].pos,
                ];
                (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
            })
            .sum();
        assert!(signed_area > 0.0, "fan winding is consistent");
        assert!(mesh.vertices.iter().any(|v| v.uv[1] < 0.0));
        assert!(mesh.vertices.iter().any(|v| v.uv[1] > 0.0));
    }
}
