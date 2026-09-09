//! Stroke geometry: turn stored point runs into render-ready polylines.
//!
//! PencilKit-authored strokes store uniform cubic B-spline *control points*;
//! rendering needs the evaluated curve. Polyline-sampled strokes pass through
//! unchanged.

use crate::stroke::{PointKind, Stroke, StrokePoint};

/// Curve samples evaluated per spline segment. 8 keeps a typical pen segment
/// (a few canvas units long) visually smooth at 1:1 zoom.
const SAMPLES_PER_SEGMENT: u32 = 8;

/// Evaluate a stroke's stored points into a drawable polyline.
///
/// B-spline control runs are clamped (first/last control point repeated) so
/// the curve interpolates the stroke's endpoints, matching how PencilKit
/// renders its own paths. Force and timestamps are blended with the same
/// basis, so width transitions stay smooth.
pub fn flatten_stroke(stroke: &Stroke) -> Vec<StrokePoint> {
    match stroke.kind {
        PointKind::PolylineSample => stroke.points.clone(),
        PointKind::BSplineControl => flatten_bspline(&stroke.points),
    }
}

fn flatten_bspline(control: &[StrokePoint]) -> Vec<StrokePoint> {
    if control.len() < 2 {
        return control.to_vec();
    }

    // Clamp by repeating the endpoints to multiplicity 3.
    let first = control[0];
    let last = control[control.len() - 1];
    let padded: Vec<StrokePoint> = [first, first]
        .into_iter()
        .chain(control.iter().copied())
        .chain([last, last])
        .collect();

    let segments = padded.len() - 3;
    let mut out = Vec::with_capacity(segments * SAMPLES_PER_SEGMENT as usize + 1);
    for seg in 0..segments {
        let window = &padded[seg..seg + 4];
        // Skip t=1.0 except on the final segment: it equals the next
        // segment's t=0.0.
        let last_sample = if seg + 1 == segments {
            SAMPLES_PER_SEGMENT
        } else {
            SAMPLES_PER_SEGMENT - 1
        };
        for i in 0..=last_sample {
            let t = i as f32 / SAMPLES_PER_SEGMENT as f32;
            out.push(eval_segment(window, t));
        }
    }
    out
}

/// Uniform cubic B-spline basis over one 4-point window.
fn eval_segment(w: &[StrokePoint], t: f32) -> StrokePoint {
    let t2 = t * t;
    let t3 = t2 * t;
    let b = [
        (1.0 - t).powi(3) / 6.0,
        (3.0 * t3 - 6.0 * t2 + 4.0) / 6.0,
        (-3.0 * t3 + 3.0 * t2 + 3.0 * t + 1.0) / 6.0,
        t3 / 6.0,
    ];
    let blend =
        |f: fn(&StrokePoint) -> f32| -> f32 { b.iter().zip(w).map(|(b, p)| b * f(p)).sum::<f32>() };
    StrokePoint {
        x: blend(|p| p.x),
        y: blend(|p| p.y),
        force: blend(|p| p.force).clamp(0.0, 1.0),
        t_ms: b
            .iter()
            .zip(w)
            .map(|(b, p)| b * p.t_ms as f32)
            .sum::<f32>()
            .round() as u32,
        tilt: None,
        // Sizes blend only when the whole window carries them (PencilKit
        // strokes are all-or-nothing per chunk, so mixed windows are rare
        // and only occur at chunk boundaries of hand-built strokes).
        size: w
            .iter()
            .all(|p| p.size.is_some())
            .then(|| crate::PointSize {
                w: blend(|p| p.size.map_or(0.0, |s| s.w)),
                h: blend(|p| p.size.map_or(0.0, |s| s.h)),
            }),
    }
}

// ---- ribbon tessellation ----

/// Pressure below this still leaves visible ink.
const MIN_FORCE: f32 = 0.15;
/// Points closer than this (canvas units) are merged before tessellation.
const MIN_SEGMENT: f32 = 0.05;

/// A stroke tessellated into a triangle ribbon, in canvas space
/// (x right, y down). Renderers flip y as their convention requires.
pub struct RibbonMesh {
    /// Two vertices per surviving source point.
    pub positions: Vec<[f32; 2]>,
    pub indices: Vec<u32>,
}

/// Half the rendered width at `p`: the point's own `size.w` when present
/// (PencilKit-authored), otherwise `base_width * force`.
fn half_width(p: &StrokePoint, base_width: f32) -> f32 {
    match p.size {
        Some(s) => (s.w / 2.0).max(0.05),
        None => (base_width * p.force.clamp(MIN_FORCE, 1.0) / 2.0).max(0.05),
    }
}

/// Tessellate a flattened polyline into a variable-width ribbon; width per
/// point is the point's own `size.w` when present (PencilKit-authored),
/// otherwise `base_width * force`. A single point (or all-coincident points)
/// becomes a small quad so dots still render.
pub fn ribbon(points: &[StrokePoint], base_width: f32) -> RibbonMesh {
    let pts = dedupe(points);
    let half = |p: &StrokePoint| half_width(p, base_width);

    match pts.as_slice() {
        [] => RibbonMesh {
            positions: Vec::new(),
            indices: Vec::new(),
        },
        [p] => {
            let h = half(p);
            RibbonMesh {
                positions: vec![
                    [p.x - h, p.y - h],
                    [p.x + h, p.y - h],
                    [p.x - h, p.y + h],
                    [p.x + h, p.y + h],
                ],
                indices: vec![0, 1, 2, 2, 1, 3],
            }
        }
        pts => {
            let mut positions = Vec::with_capacity(pts.len() * 2);
            for (i, p) in pts.iter().enumerate() {
                let prev = &pts[i.saturating_sub(1)];
                let next = &pts[(i + 1).min(pts.len() - 1)];
                let (dx, dy) = (next.x - prev.x, next.y - prev.y);
                let len = (dx * dx + dy * dy).sqrt().max(f32::EPSILON);
                // Left-hand normal of the averaged direction.
                let (nx, ny) = (-dy / len, dx / len);
                let h = half(p);
                positions.push([p.x + nx * h, p.y + ny * h]);
                positions.push([p.x - nx * h, p.y - ny * h]);
            }
            let mut indices = Vec::with_capacity((pts.len() - 1) * 6);
            for i in 0..pts.len() as u32 - 1 {
                let base = i * 2;
                indices.extend([base, base + 1, base + 2, base + 2, base + 1, base + 3]);
            }
            RibbonMesh { positions, indices }
        }
    }
}

/// The ribbon as a flat list of triangles (three `[x, y]` per triangle),
/// every triangle wound the same way. Renderers that fill a path with the
/// non-zero rule (CoreGraphics, SVG, Skia) get exactly the mesh's coverage:
/// same-winding overlaps add up instead of cancelling into holes.
pub fn ribbon_triangles(points: &[StrokePoint], base_width: f32) -> Vec<[f32; 2]> {
    let mesh = ribbon(points, base_width);
    let mut out = Vec::with_capacity(mesh.indices.len());
    for tri in mesh.indices.as_chunks::<3>().0 {
        let (a, b, c) = (
            mesh.positions[tri[0] as usize],
            mesh.positions[tri[1] as usize],
            mesh.positions[tri[2] as usize],
        );
        let twice_area = (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]);
        if twice_area >= 0.0 {
            out.extend([a, b, c]);
        } else {
            out.extend([a, c, b]);
        }
    }
    out
}

/// Whole-stroke hit test: does a circle of `radius` at (`x`, `y`) touch the
/// ink of this flattened polyline? Used by the eraser on every platform so
/// erasing behaves the same everywhere.
pub fn hits(points: &[StrokePoint], base_width: f32, x: f32, y: f32, radius: f32) -> bool {
    let pts = dedupe(points);
    let within = |d2: f32, reach: f32| d2 <= reach * reach;
    match pts.as_slice() {
        [] => false,
        [p] => within(
            (p.x - x).powi(2) + (p.y - y).powi(2),
            radius + half_width(p, base_width),
        ),
        pts => pts.windows(2).any(|w| {
            let (a, b) = (&w[0], &w[1]);
            let (abx, aby) = (b.x - a.x, b.y - a.y);
            let len2 = (abx * abx + aby * aby).max(f32::EPSILON);
            let t = (((x - a.x) * abx + (y - a.y) * aby) / len2).clamp(0.0, 1.0);
            let (cx, cy) = (a.x + t * abx, a.y + t * aby);
            let reach = radius + half_width(a, base_width).max(half_width(b, base_width));
            within((cx - x).powi(2) + (cy - y).powi(2), reach)
        }),
    }
}

fn dedupe(points: &[StrokePoint]) -> Vec<StrokePoint> {
    let mut out: Vec<StrokePoint> = Vec::with_capacity(points.len());
    for p in points {
        if let Some(last) = out.last()
            && (p.x - last.x).abs() < MIN_SEGMENT
            && (p.y - last.y).abs() < MIN_SEGMENT
        {
            continue;
        }
        out.push(*p);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StrokeId;
    use crate::stroke::{Rgba, Tool};

    fn pt(x: f32, y: f32) -> StrokePoint {
        StrokePoint {
            x,
            y,
            force: 0.5,
            t_ms: 0,
            tilt: None,
            size: None,
        }
    }

    fn stroke(kind: PointKind, points: Vec<StrokePoint>) -> Stroke {
        Stroke {
            id: StrokeId::new(),
            tool: Tool::Pen,
            color: Rgba::BLACK,
            base_width: 2.0,
            kind,
            points,
            created_ms: 0,
        }
    }

    #[test]
    fn polyline_passes_through() {
        let points = vec![pt(0.0, 0.0), pt(3.0, 4.0)];
        let s = stroke(PointKind::PolylineSample, points.clone());
        assert_eq!(flatten_stroke(&s), points);
    }

    #[test]
    fn clamped_bspline_hits_endpoints() {
        let s = stroke(
            PointKind::BSplineControl,
            vec![pt(0.0, 0.0), pt(10.0, 0.0), pt(10.0, 10.0), pt(0.0, 10.0)],
        );
        let flat = flatten_stroke(&s);
        let first = flat.first().unwrap();
        let last = flat.last().unwrap();
        assert!((first.x - 0.0).abs() < 1e-3 && (first.y - 0.0).abs() < 1e-3);
        assert!((last.x - 0.0).abs() < 1e-3 && (last.y - 10.0).abs() < 1e-3);
        assert!(flat.len() > s.points.len());
    }

    #[test]
    fn bspline_stays_in_control_hull() {
        let s = stroke(
            PointKind::BSplineControl,
            vec![pt(0.0, 0.0), pt(5.0, 8.0), pt(12.0, -3.0), pt(20.0, 4.0)],
        );
        // Convex-combination bound, with float-error slack.
        for p in flatten_stroke(&s) {
            assert!((-1e-3..=20.0 + 1e-3).contains(&p.x), "x = {}", p.x);
            assert!((-3.0 - 1e-3..=8.0 + 1e-3).contains(&p.y), "y = {}", p.y);
            assert!((0.0..=1.0).contains(&p.force));
        }
    }

    fn fpt(x: f32, y: f32, force: f32) -> StrokePoint {
        StrokePoint {
            x,
            y,
            force,
            t_ms: 0,
            tilt: None,
            size: None,
        }
    }

    #[test]
    fn line_makes_two_triangles_per_segment() {
        let mesh = ribbon(
            &[fpt(0.0, 0.0, 1.0), fpt(10.0, 0.0, 1.0), fpt(20.0, 0.0, 1.0)],
            2.0,
        );
        assert_eq!(mesh.positions.len(), 6);
        assert_eq!(mesh.indices.len(), 12);
        // Horizontal line: normals are vertical, full width at force 1.
        assert!((mesh.positions[0][1] - 1.0).abs() < 1e-4);
        assert!((mesh.positions[1][1] + 1.0).abs() < 1e-4);
    }

    #[test]
    fn pressure_narrows_the_ribbon() {
        let mesh = ribbon(&[fpt(0.0, 0.0, 0.5), fpt(10.0, 0.0, 0.5)], 4.0);
        let width = (mesh.positions[0][1] - mesh.positions[1][1]).abs();
        assert!((width - 2.0).abs() < 1e-4);
    }

    #[test]
    fn single_point_renders_as_dot() {
        let mesh = ribbon(&[fpt(5.0, 5.0, 1.0)], 2.0);
        assert_eq!(mesh.positions.len(), 4);
        assert_eq!(mesh.indices.len(), 6);
    }

    #[test]
    fn coincident_points_collapse_to_a_dot() {
        let mesh = ribbon(
            &[fpt(1.0, 1.0, 1.0), fpt(1.0, 1.0, 1.0), fpt(1.0, 1.0, 1.0)],
            2.0,
        );
        assert_eq!(mesh.positions.len(), 4);
    }

    #[test]
    fn empty_input_is_empty() {
        let mesh = ribbon(&[], 2.0);
        assert!(mesh.positions.is_empty() && mesh.indices.is_empty());
    }

    #[test]
    fn triangles_all_wound_the_same_way() {
        // A sharp hairpin folds the ribbon; the strip's triangles flip.
        let pts = [
            fpt(0.0, 0.0, 1.0),
            fpt(10.0, 0.0, 1.0),
            fpt(10.0, 0.5, 1.0),
            fpt(0.0, 1.0, 1.0),
        ];
        let tris = ribbon_triangles(&pts, 4.0);
        assert_eq!(tris.len(), ribbon(&pts, 4.0).indices.len());
        for tri in tris.as_chunks::<3>().0 {
            let (a, b, c) = (tri[0], tri[1], tri[2]);
            let twice_area = (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]);
            assert!(twice_area >= 0.0, "clockwise triangle {tri:?}");
        }
    }

    #[test]
    fn hit_test_respects_width_and_radius() {
        let line = [fpt(0.0, 0.0, 1.0), fpt(10.0, 0.0, 1.0)];
        // Ink is 2 wide (half = 1). 0.5 away with no radius: hit.
        assert!(hits(&line, 2.0, 5.0, 0.5, 0.0));
        // 3 away: miss with radius 1, hit with radius 2.5.
        assert!(!hits(&line, 2.0, 5.0, 3.0, 1.0));
        assert!(hits(&line, 2.0, 5.0, 3.0, 2.5));
        // Beyond the endpoint along the line: distance is to the cap.
        assert!(!hits(&line, 2.0, 13.0, 0.0, 1.0));
        assert!(hits(&line, 2.0, 12.5, 0.0, 2.0));
        // A dot.
        assert!(hits(&[fpt(3.0, 3.0, 1.0)], 2.0, 3.5, 3.0, 0.0));
        assert!(!hits(&[], 2.0, 0.0, 0.0, 100.0));
    }
}
