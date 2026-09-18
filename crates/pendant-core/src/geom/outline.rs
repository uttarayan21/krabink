//! A stroke's outline as one closed polygon, for renderers that fill
//! paths rather than triangles (SVG export). Filled with the nonzero rule
//! a single polygon never darkens where it overlaps itself, which is the
//! same look [`crate::Overlap::Discard`] gives on the GPU.

use super::MIN_SEGMENT;
use crate::brush::{MIN_TIP, TipState};

/// Segments per full circle of a round cap.
const CAP_SEGMENTS: usize = 16;

/// Left edge forward, round end cap, right edge back, round start cap.
/// Oriented tips get flat ends (the nib) instead of arcs. Empty for no
/// points.
pub fn outline_polygon(pts: &[([f32; 2], TipState)], nib: bool) -> Vec<[f32; 2]> {
    let Some(&(first, first_tip)) = pts.first() else {
        return Vec::new();
    };
    if pts.len() == 1 {
        return if nib {
            rect(first, &first_tip)
        } else {
            circle(first, first_tip.w / 2.0)
        };
    }

    let n = pts.len();
    let offsets: Vec<[f32; 2]> = (0..n)
        .map(|i| {
            let (_, tip) = pts[i];
            let prev = pts[i.saturating_sub(1)].0;
            let next = pts[(i + 1).min(n - 1)].0;
            let (dx, dy) = (next[0] - prev[0], next[1] - prev[1]);
            let len = (dx * dx + dy * dy).sqrt().max(f32::EPSILON);
            let (nx, ny) = (-dy / len, dx / len);
            if nib {
                let (ux, uy) = (tip.rot.cos(), tip.rot.sin());
                let half = (tip.w / 2.0).max(MIN_TIP);
                let side = if ux * nx + uy * ny < 0.0 { -1.0 } else { 1.0 };
                let across = (ux * nx + uy * ny).abs() * half;
                let floor = (tip.w * super::nib::NIB_MIN_FRACTION / 2.0).max(MIN_TIP);
                if across < floor {
                    [nx * floor, ny * floor]
                } else {
                    [ux * half * side, uy * half * side]
                }
            } else {
                let half = (tip.w / 2.0).max(MIN_TIP);
                [nx * half, ny * half]
            }
        })
        .collect();

    let mut out = Vec::with_capacity(2 * n + 2 * CAP_SEGMENTS);
    out.extend(
        pts.iter()
            .zip(&offsets)
            .map(|((p, _), o)| [p[0] + o[0], p[1] + o[1]]),
    );
    let (last, last_tip) = pts[n - 1];
    if !nib {
        let dir = direction(pts[n - 2].0, last);
        out.extend(arc(last, last_tip.w / 2.0, dir, offsets[n - 1]));
    }
    out.extend(
        pts.iter()
            .zip(&offsets)
            .rev()
            .map(|((p, _), o)| [p[0] - o[0], p[1] - o[1]]),
    );
    if !nib {
        let dir = direction(pts[1].0, first);
        out.extend(arc(
            first,
            first_tip.w / 2.0,
            dir,
            [-offsets[0][0], -offsets[0][1]],
        ));
    }
    out
}

fn direction(from: [f32; 2], to: [f32; 2]) -> [f32; 2] {
    let (dx, dy) = (to[0] - from[0], to[1] - from[1]);
    let len = (dx * dx + dy * dy).sqrt().max(MIN_SEGMENT);
    [dx / len, dy / len]
}

/// Half circle of `radius` about `c`, from the edge at `start` (a vector
/// from `c`) sweeping through `dir` to the opposite edge.
fn arc(c: [f32; 2], radius: f32, dir: [f32; 2], start: [f32; 2]) -> Vec<[f32; 2]> {
    let radius = radius.max(MIN_TIP);
    let a0 = start[1].atan2(start[0]);
    // Sweep the half turn that passes through `dir`.
    let mid = dir[1].atan2(dir[0]);
    let delta = mid - a0;
    let sweep = if delta.rem_euclid(core::f32::consts::TAU) < core::f32::consts::PI {
        core::f32::consts::PI
    } else {
        -core::f32::consts::PI
    };
    let steps = CAP_SEGMENTS / 2;
    (1..steps)
        .map(|i| {
            // usize -> f32 exact for these small counts.
            let t = i as f32 / steps as f32; // ast-grep-ignore: no-as-cast
            let a = a0 + sweep * t;
            [c[0] + radius * a.cos(), c[1] + radius * a.sin()]
        })
        .collect()
}

fn circle(c: [f32; 2], radius: f32) -> Vec<[f32; 2]> {
    let radius = radius.max(MIN_TIP);
    (0..CAP_SEGMENTS)
        .map(|i| {
            let a = core::f32::consts::TAU * i as f32 / CAP_SEGMENTS as f32; // ast-grep-ignore: no-as-cast
            [c[0] + radius * a.cos(), c[1] + radius * a.sin()]
        })
        .collect()
}

fn rect(c: [f32; 2], tip: &TipState) -> Vec<[f32; 2]> {
    let (ux, uy) = (tip.rot.cos(), tip.rot.sin());
    let (l, t) = ((tip.w / 2.0).max(MIN_TIP), (tip.h / 2.0).max(MIN_TIP));
    vec![
        [c[0] + ux * l - uy * t, c[1] + uy * l + ux * t],
        [c[0] - ux * l - uy * t, c[1] - uy * l + ux * t],
        [c[0] - ux * l + uy * t, c[1] - uy * l - ux * t],
        [c[0] + ux * l + uy * t, c[1] + uy * l - ux * t],
    ]
}
