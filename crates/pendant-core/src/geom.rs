//! Stroke geometry: turn stored point runs into render-ready meshes.
//!
//! Two stages. [`Stroke::flatten`] evaluates PencilKit-authored uniform cubic
//! B-spline *control points* into a polyline (polyline-sampled strokes pass
//! through unchanged). [`stroke_mesh`] then tessellates that polyline into
//! triangles: round tools go through lyon's stroker with a per-point width
//! attribute, round caps and round joins; the flat-nib brush keeps its own
//! orientation-driven ribbon, which lyon has no notion of. Every renderer
//! (bevy on the desktop, Metal/CoreGraphics on iPad, the SVG exporter)
//! draws the same mesh, so ink looks identical everywhere.

use lyon_tessellation::{
    BuffersBuilder, LineCap, LineJoin, StrokeOptions, StrokeTessellator, StrokeVertex,
    VertexBuffers, math::point, path::Path,
};

use crate::stroke::{PointKind, Stroke, StrokePoint, Tool};

/// Curve samples evaluated per spline segment. 8 keeps a typical pen segment
/// (a few canvas units long) visually smooth at 1:1 zoom.
const SAMPLES_PER_SEGMENT: u32 = 8;

impl Stroke {
    /// Evaluate the stored points into a drawable polyline.
    ///
    /// B-spline control runs are clamped (first/last control point repeated)
    /// so the curve interpolates the stroke's endpoints, matching how
    /// PencilKit renders its own paths. Force and timestamps are blended
    /// with the same basis, so width transitions stay smooth.
    pub fn flatten(&self) -> Vec<StrokePoint> {
        match self.kind {
            PointKind::PolylineSample => self.points.clone(),
            PointKind::BSplineControl => flatten_bspline(&self.points),
        }
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

// ---- tessellation ----

/// Pressure below this still leaves visible ink.
const MIN_FORCE: f32 = 0.15;
/// Points closer than this (canvas units) are merged before tessellation.
const MIN_SEGMENT: f32 = 0.05;
/// Narrowest ink that still gets a mesh, canvas units (half width).
const MIN_HALF_WIDTH: f32 = 0.05;
/// Flattening tolerance for caps and joins at 1:1 zoom, canvas units.
/// Callers zoomed in by `z` should pass `DEFAULT_TOLERANCE / z`.
pub const DEFAULT_TOLERANCE: f32 = 0.25;

/// A stroke tessellated into triangles, in canvas space (x right, y down).
/// Renderers flip y as their convention requires.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StrokeMesh {
    pub positions: Vec<[f32; 2]>,
    /// Triangle list into `positions`.
    pub indices: Vec<u32>,
}

impl StrokeMesh {
    /// Nothing to draw.
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    /// Axis-aligned bounds as `(min, max)`, or `None` when empty.
    pub fn bounds(&self) -> Option<([f32; 2], [f32; 2])> {
        self.positions.iter().fold(None, |acc, [x, y]| {
            let (lo, hi) = acc.unwrap_or(([*x, *y], [*x, *y]));
            Some((
                [lo[0].min(*x), lo[1].min(*y)],
                [hi[0].max(*x), hi[1].max(*y)],
            ))
        })
    }
}

/// Half the rendered width at `p`: the point's own `size.w` when present
/// (PencilKit-authored), otherwise `base_width * force`.
fn half_width(p: &StrokePoint, base_width: f32) -> f32 {
    match p.size {
        Some(s) => (s.w / 2.0).max(MIN_HALF_WIDTH),
        None => (base_width * p.force.clamp(MIN_FORCE, 1.0) / 2.0).max(MIN_HALF_WIDTH),
    }
}

/// Thinnest a flat nib gets when dragged along its own length, as a
/// fraction of `base_width`.
const NIB_MIN_FRACTION: f32 = 0.08;

/// Tessellate a flattened polyline for `tool`. Round tools get lyon's
/// variable-width stroker with round caps and joins ([`round_mesh`]); a nib
/// tool offsets along the nib's own orientation ([`nib_ribbon`]), which is
/// what makes calligraphic strokes thick across the nib and thin along it.
/// `tolerance` bounds the flattening error of caps and joins in canvas
/// units; see [`DEFAULT_TOLERANCE`].
pub fn stroke_mesh(
    tool: Tool,
    points: &[StrokePoint],
    base_width: f32,
    tolerance: f32,
) -> StrokeMesh {
    if tool.has_nib() {
        nib_ribbon(points, base_width)
    } else {
        round_mesh(points, base_width, tolerance)
    }
}

/// Variable-width stroke through lyon: width per point is the point's own
/// `size.w` when present, otherwise `base_width * force`; caps and joins
/// are round. A single point (or all-coincident points) becomes a dot of
/// the point's width.
fn round_mesh(points: &[StrokePoint], base_width: f32, tolerance: f32) -> StrokeMesh {
    let pts = dedupe(points);
    let Some(first) = pts.first() else {
        return StrokeMesh::default();
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
    let mut buffers: VertexBuffers<[f32; 2], u32> = VertexBuffers::new();
    let result = StrokeTessellator::new().tessellate_path(
        &path,
        &options,
        &mut BuffersBuilder::new(&mut buffers, |v: StrokeVertex| {
            let p = v.position();
            [p.x, p.y]
        }),
    );
    if let Err(err) = result {
        tracing::warn!(?err, points = pts.len(), "stroke tessellation failed");
        return StrokeMesh::default();
    }
    StrokeMesh {
        positions: buffers.vertices,
        indices: buffers.indices,
    }
}

/// Flat-nib tessellation: each point becomes the two ends of a `base_width`
/// long nib turned to `tilt.nib_angle()` (azimuth + barrel roll). Points
/// without orientation fall back to the motion normal, so a stroke from a
/// pen that cannot report roll still renders.
pub fn nib_ribbon(points: &[StrokePoint], base_width: f32) -> StrokeMesh {
    let pts = dedupe(points);
    let half = (base_width / 2.0).max(MIN_HALF_WIDTH);
    let floor = (base_width * NIB_MIN_FRACTION / 2.0).max(MIN_HALF_WIDTH);
    match pts.as_slice() {
        [] => return StrokeMesh::default(),
        [p] => {
            // A nib touched down without moving: a square dot of its width.
            let h = half;
            return StrokeMesh {
                positions: vec![
                    [p.x - h, p.y - h],
                    [p.x + h, p.y - h],
                    [p.x - h, p.y + h],
                    [p.x + h, p.y + h],
                ],
                indices: vec![0, 1, 2, 2, 1, 3],
            };
        }
        _ => {}
    }
    let mut positions = Vec::with_capacity(pts.len() * 2);
    for (i, p) in pts.iter().enumerate() {
        let prev = &pts[i.saturating_sub(1)];
        let next = &pts[(i + 1).min(pts.len() - 1)];
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
        positions.push([p.x + ox, p.y + oy]);
        positions.push([p.x - ox, p.y - oy]);
    }
    StrokeMesh {
        indices: strip_indices(pts.len()),
        positions,
    }
}

/// Triangle-list indices for a strip of `points` (left, right) vertex pairs.
fn strip_indices(points: usize) -> Vec<u32> {
    (0..points.saturating_sub(1) as u32)
        .flat_map(|i| {
            let base = i * 2;
            [base, base + 1, base + 2, base + 2, base + 1, base + 3]
        })
        .collect()
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

/// Drop non-finite points and merge runs closer than [`MIN_SEGMENT`].
fn dedupe(points: &[StrokePoint]) -> Vec<StrokePoint> {
    points
        .iter()
        .filter(|p| p.x.is_finite() && p.y.is_finite() && p.force.is_finite())
        .fold(Vec::with_capacity(points.len()), |mut out, p| {
            let near = out.last().is_some_and(|last: &StrokePoint| {
                (p.x - last.x).abs() < MIN_SEGMENT && (p.y - last.y).abs() < MIN_SEGMENT
            });
            if !near {
                out.push(*p);
            }
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StrokeId;
    use crate::stroke::{Rgba, Tool};
    use proptest::prelude::*;

    fn pt(x: f32, y: f32) -> StrokePoint {
        fpt(x, y, 0.5)
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

    fn pen(points: &[StrokePoint], base_width: f32) -> StrokeMesh {
        stroke_mesh(Tool::Pen, points, base_width, DEFAULT_TOLERANCE)
    }

    fn bounds(mesh: &StrokeMesh) -> ([f32; 2], [f32; 2]) {
        mesh.bounds().expect("mesh has vertices")
    }

    #[test]
    fn polyline_passes_through() {
        let points = vec![pt(0.0, 0.0), pt(3.0, 4.0)];
        let s = stroke(PointKind::PolylineSample, points.clone());
        assert_eq!(s.flatten(), points);
    }

    #[test]
    fn clamped_bspline_hits_endpoints() {
        let s = stroke(
            PointKind::BSplineControl,
            vec![pt(0.0, 0.0), pt(10.0, 0.0), pt(10.0, 10.0), pt(0.0, 10.0)],
        );
        let flat = s.flatten();
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
        for p in s.flatten() {
            assert!((-1e-3..=20.0 + 1e-3).contains(&p.x), "x = {}", p.x);
            assert!((-3.0 - 1e-3..=8.0 + 1e-3).contains(&p.y), "y = {}", p.y);
            assert!((0.0..=1.0).contains(&p.force));
        }
    }

    #[test]
    fn line_gets_full_width_and_round_caps() {
        let mesh = pen(&[fpt(0.0, 0.0, 1.0), fpt(10.0, 0.0, 1.0)], 2.0);
        let (lo, hi) = bounds(&mesh);
        // Ink is 2 wide: y spans ±1 …
        assert!(
            (lo[1] + 1.0).abs() < 1e-3 && (hi[1] - 1.0).abs() < 1e-3,
            "{lo:?} {hi:?}"
        );
        // … and round caps reach half a width past each endpoint.
        assert!(
            (lo[0] + 1.0).abs() < 0.05 && (hi[0] - 11.0).abs() < 0.05,
            "{lo:?} {hi:?}"
        );
        // A cap is an arc, not a butt: some vertex lies beyond x=10 but
        // strictly inside the full width.
        assert!(
            mesh.positions
                .iter()
                .any(|[x, y]| *x > 10.05 && y.abs() < 0.95),
            "no cap arc vertex"
        );
    }

    #[test]
    fn pressure_narrows_the_ink() {
        let mesh = pen(&[fpt(0.0, 0.0, 0.5), fpt(10.0, 0.0, 0.5)], 4.0);
        let (lo, hi) = bounds(&mesh);
        assert!((hi[1] - lo[1] - 2.0).abs() < 1e-3, "{lo:?} {hi:?}");
    }

    #[test]
    fn width_varies_along_the_stroke() {
        let mesh = pen(&[fpt(0.0, 0.0, 0.25), fpt(40.0, 0.0, 1.0)], 4.0);
        let thin = mesh
            .positions
            .iter()
            .filter(|[x, _]| (0.0..5.0).contains(x))
            .map(|[_, y]| y.abs())
            .fold(0.0_f32, f32::max);
        let thick = mesh
            .positions
            .iter()
            .filter(|[x, _]| (35.0..=40.0).contains(x))
            .map(|[_, y]| y.abs())
            .fold(0.0_f32, f32::max);
        assert!(
            thin < thick,
            "thin end {thin} should be narrower than {thick}"
        );
    }

    #[test]
    fn stored_size_overrides_force() {
        let sized = |x: f32| StrokePoint {
            size: Some(crate::PointSize { w: 6.0, h: 6.0 }),
            ..fpt(x, 0.0, 0.1)
        };
        let mesh = pen(&[sized(0.0), sized(10.0)], 2.0);
        let (lo, hi) = bounds(&mesh);
        assert!((hi[1] - lo[1] - 6.0).abs() < 1e-3, "{lo:?} {hi:?}");
    }

    #[test]
    fn single_point_renders_as_dot() {
        let mesh = pen(&[fpt(5.0, 5.0, 1.0)], 2.0);
        assert!(!mesh.is_empty());
        let (lo, hi) = bounds(&mesh);
        assert!(
            (lo[0] - 4.0).abs() < 0.1 && (hi[0] - 6.0).abs() < 0.1,
            "{lo:?} {hi:?}"
        );
        assert!(
            (lo[1] - 4.0).abs() < 0.1 && (hi[1] - 6.0).abs() < 0.1,
            "{lo:?} {hi:?}"
        );
    }

    #[test]
    fn coincident_points_collapse_to_a_dot() {
        let dot = pen(&[fpt(1.0, 1.0, 1.0)], 2.0);
        let mesh = pen(
            &[fpt(1.0, 1.0, 1.0), fpt(1.0, 1.0, 1.0), fpt(1.0, 1.0, 1.0)],
            2.0,
        );
        assert_eq!(mesh, dot);
    }

    #[test]
    fn empty_and_non_finite_input_is_empty() {
        assert!(pen(&[], 2.0).is_empty());
        assert!(
            pen(
                &[fpt(f32::NAN, 0.0, 1.0), fpt(1.0, f32::INFINITY, 1.0)],
                2.0
            )
            .is_empty()
        );
    }

    #[test]
    fn hairpin_keeps_ink_at_the_turn() {
        // A sharp reversal: butt ribbons fold into a hole here; a round
        // join must leave ink covering the corner.
        let pts = [fpt(0.0, 0.0, 1.0), fpt(10.0, 0.0, 1.0), fpt(0.0, 0.5, 1.0)];
        let mesh = pen(&pts, 4.0);
        let (_, hi) = bounds(&mesh);
        assert!(
            hi[0] > 11.5,
            "join should bulge past the corner, got {hi:?}"
        );
    }

    #[test]
    fn finer_tolerance_adds_vertices() {
        let pts = [fpt(0.0, 0.0, 1.0), fpt(10.0, 0.0, 1.0)];
        let coarse = stroke_mesh(Tool::Pen, &pts, 8.0, 1.0);
        let fine = stroke_mesh(Tool::Pen, &pts, 8.0, 0.01);
        assert!(fine.positions.len() > coarse.positions.len());
    }

    #[test]
    fn nib_width_follows_orientation() {
        use crate::stroke::Tilt;
        let with_nib = |x: f32, angle: f32| StrokePoint {
            tilt: Some(Tilt {
                azimuth: angle,
                altitude: 0.5,
                roll: 0.0,
            }),
            ..fpt(x, 0.0, 1.0)
        };
        let half_pi = core::f32::consts::FRAC_PI_2;
        // Nib across the motion (pointing +y on a horizontal stroke): full width.
        let across = nib_ribbon(&[with_nib(0.0, half_pi), with_nib(10.0, half_pi)], 8.0);
        let w = (across.positions[0][1] - across.positions[1][1]).abs();
        assert!((w - 8.0).abs() < 1e-3, "across width {w}");
        // Nib along the motion: only the thin floor remains.
        let along = nib_ribbon(&[with_nib(0.0, 0.0), with_nib(10.0, 0.0)], 8.0);
        let w = (along.positions[0][1] - along.positions[1][1]).abs();
        assert!((w - 8.0 * NIB_MIN_FRACTION).abs() < 1e-3, "along width {w}");
        // Roll turns the nib: azimuth 0 rolled by π/2 is across again.
        let rolled = |x: f32| StrokePoint {
            tilt: Some(Tilt {
                azimuth: 0.0,
                altitude: 0.5,
                roll: half_pi,
            }),
            ..fpt(x, 0.0, 1.0)
        };
        let rolled = nib_ribbon(&[rolled(0.0), rolled(10.0)], 8.0);
        let w = (rolled.positions[0][1] - rolled.positions[1][1]).abs();
        assert!((w - 8.0).abs() < 1e-3, "rolled width {w}");
        // The brush tool routes to the nib; round tools ignore it.
        let nib_pts = [with_nib(0.0, 0.0), with_nib(10.0, 0.0)];
        assert_eq!(
            stroke_mesh(Tool::Brush, &nib_pts, 8.0, DEFAULT_TOLERANCE),
            nib_ribbon(&nib_pts, 8.0)
        );
        assert_eq!(
            stroke_mesh(Tool::Pen, &nib_pts, 8.0, DEFAULT_TOLERANCE),
            pen(&[fpt(0.0, 0.0, 1.0), fpt(10.0, 0.0, 1.0)], 8.0)
        );
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

    fn arb_point() -> impl Strategy<Value = StrokePoint> {
        (
            -2000.0_f32..2000.0,
            -2000.0_f32..2000.0,
            0.0_f32..=1.0,
            proptest::option::of(0.1_f32..40.0),
        )
            .prop_map(|(x, y, force, w)| StrokePoint {
                size: w.map(|w| crate::PointSize { w, h: w }),
                ..fpt(x, y, force)
            })
    }

    proptest! {
        #[test]
        fn any_stroke_tessellates_finitely(
            pts in proptest::collection::vec(arb_point(), 0..64),
            base in 0.5_f32..40.0,
            tol in 0.01_f32..2.0,
        ) {
            for tool in [Tool::Pen, Tool::Marker, Tool::Monoline, Tool::Brush] {
                let mesh = stroke_mesh(tool, &pts, base, tol);
                prop_assert_eq!(mesh.indices.len() % 3, 0);
                prop_assert!(mesh.positions.iter().all(|[x, y]| x.is_finite() && y.is_finite()));
                prop_assert!(mesh.indices.iter().all(|i| (*i as usize) < mesh.positions.len()));
                prop_assert_eq!(mesh.is_empty(), pts.is_empty());
            }
        }
    }
}
