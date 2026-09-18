//! Stroke geometry: turn stored point runs into render-ready meshes.
//!
//! [`Stroke::flatten`] evaluates PencilKit-authored uniform cubic B-spline
//! *control points* into a polyline (polyline-sampled strokes pass through
//! unchanged). [`Ink::mesh`] folds that polyline through the brush's
//! [`TipEvaluator`] and tessellates the tip states into an [`InkMesh`]:
//! round tips go through lyon's stroker with width and opacity attributes,
//! round caps and round joins ([`continuous`]); oriented tips (chisel,
//! nib) get a ribbon with the nib's own rectangle at every point ([`nib`]).
//! Every renderer (bevy on the desktop, Metal on iPad, the SVG exporter)
//! draws the same mesh, so ink looks identical everywhere.

mod continuous;
mod mesh;
mod nib;
mod outline;

use std::borrow::Cow;

pub use mesh::{InkMesh, InkStyle, InkVertex};

use crate::brush::{BrushSpec, StrokeEnd, TipEvaluator, TipState};
use crate::stroke::{PointKind, Rgba, Stroke, StrokePoint, Tool};

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

    /// How this stroke is inked.
    pub fn ink(&self) -> Ink<'_> {
        Ink {
            spec: self.spec(),
            color: self.color,
            base_width: self.base_width,
        }
    }

    /// This stroke's committed ink.
    pub fn mesh(&self, tolerance: f32) -> InkMesh {
        self.ink()
            .mesh(&self.flatten(), StrokeEnd::Complete, tolerance)
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

/// Points closer than this (canvas units) are merged before tessellation.
pub(crate) const MIN_SEGMENT: f32 = 0.05;
/// Flattening tolerance for caps and joins at 1:1 zoom, canvas units.
/// Callers zoomed in by `z` should pass `DEFAULT_TOLERANCE / z`.
pub const DEFAULT_TOLERANCE: f32 = 0.25;

/// A brush applied at a colour and size: everything geometry needs besides
/// the points. Borrow a stroke's custom spec or own a preset.
#[derive(Debug, Clone, PartialEq)]
pub struct Ink<'a> {
    pub spec: Cow<'a, BrushSpec>,
    pub color: Rgba,
    /// Full ink width in canvas units, the brush "size".
    pub base_width: f32,
}

impl Ink<'static> {
    /// A built-in tool's preset at `color` and `base_width`.
    pub fn preset(tool: Tool, color: Rgba, base_width: f32) -> Self {
        Self {
            spec: Cow::Owned(BrushSpec::preset(tool)),
            color,
            base_width,
        }
    }
}

impl Ink<'_> {
    pub fn style(&self) -> InkStyle {
        InkStyle::of(self.color, &self.spec.tip, &self.spec.paint)
    }

    /// Tessellate a run of input points. `end` says whether the run is
    /// still being drawn (no end taper yet) or is the whole stroke.
    /// `tolerance` bounds the flattening error of caps and joins in canvas
    /// units; see [`DEFAULT_TOLERANCE`].
    pub fn mesh(&self, points: &[StrokePoint], end: StrokeEnd, tolerance: f32) -> InkMesh {
        let pts = self.tips(points, end);
        if self.spec.has_nib() {
            nib::nib_mesh(&pts, self.style())
        } else {
            continuous::round_mesh(&pts, self.style(), tolerance)
        }
    }

    /// The finished stroke's outline as one closed polygon, for path
    /// fillers such as SVG.
    pub fn outline(&self, points: &[StrokePoint]) -> Vec<[f32; 2]> {
        outline::outline_polygon(&self.tips(points, StrokeEnd::Complete), self.spec.has_nib())
    }

    /// Whole-stroke hit test: does a circle of `radius` at (`x`, `y`) touch
    /// the ink of this polyline? Used by the eraser on every platform so
    /// erasing behaves the same everywhere.
    pub fn hits(&self, points: &[StrokePoint], x: f32, y: f32, radius: f32) -> bool {
        let pts = self.tips(points, StrokeEnd::Complete);
        let within = |d2: f32, reach: f32| d2 <= reach * reach;
        match pts.as_slice() {
            [] => false,
            [(p, tip)] => within(
                (p[0] - x).powi(2) + (p[1] - y).powi(2),
                radius + tip.w / 2.0,
            ),
            pts => pts.windows(2).any(|w| {
                let ((a, ta), (b, tb)) = (&w[0], &w[1]);
                let reach = radius + ta.w.max(tb.w) / 2.0;
                within(segment_distance2(*a, *b, [x, y]), reach)
            }),
        }
    }

    /// Deduplicated positions paired with their tip states.
    fn tips(&self, points: &[StrokePoint], end: StrokeEnd) -> Vec<([f32; 2], TipState)> {
        let clean = dedupe(points);
        let states = TipEvaluator::evaluate(&self.spec, self.base_width, &clean, end);
        clean.iter().map(|p| [p.x, p.y]).zip(states).collect()
    }
}

/// Squared distance from `p` to the segment `a`–`b`.
pub(crate) fn segment_distance2(a: [f32; 2], b: [f32; 2], p: [f32; 2]) -> f32 {
    let (abx, aby) = (b[0] - a[0], b[1] - a[1]);
    let len2 = (abx * abx + aby * aby).max(f32::EPSILON);
    let t = (((p[0] - a[0]) * abx + (p[1] - a[1]) * aby) / len2).clamp(0.0, 1.0);
    let (cx, cy) = (a[0] + t * abx, a[1] + t * aby);
    (cx - p[0]).powi(2) + (cy - p[1]).powi(2)
}

/// Drop non-finite points and merge runs closer than [`MIN_SEGMENT`].
pub(crate) fn dedupe(points: &[StrokePoint]) -> Vec<StrokePoint> {
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
    use crate::brush::{Blend, Overlap};
    use crate::stroke::{Rgba, Tilt, Tool};
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

    /// Points spaced in time so speed thinning stays negligible.
    fn timed(points: &[StrokePoint]) -> Vec<StrokePoint> {
        points
            .iter()
            .enumerate()
            .map(|(i, p)| StrokePoint {
                t_ms: u32::try_from(i).unwrap_or(0) * 1000,
                ..*p
            })
            .collect()
    }

    fn stroke(kind: PointKind, points: Vec<StrokePoint>) -> Stroke {
        Stroke {
            id: StrokeId::new(),
            tool: Tool::Pen,
            brush: None,
            color: Rgba::BLACK,
            base_width: 2.0,
            kind,
            points,
            created_ms: 0,
        }
    }

    fn ink(tool: Tool, base_width: f32) -> Ink<'static> {
        Ink::preset(tool, Rgba::BLACK, base_width)
    }

    /// A monoline mesh: constant width, so geometry tests read cleanly.
    fn mono(points: &[StrokePoint], base_width: f32) -> InkMesh {
        ink(Tool::Monoline, base_width).mesh(points, StrokeEnd::Complete, DEFAULT_TOLERANCE)
    }

    fn bounds(mesh: &InkMesh) -> ([f32; 2], [f32; 2]) {
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
        let mesh = mono(&[fpt(0.0, 0.0, 1.0), fpt(10.0, 0.0, 1.0)], 2.0);
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
            mesh.positions().any(|[x, y]| x > 10.05 && y.abs() < 0.95),
            "no cap arc vertex"
        );
    }

    #[test]
    fn pen_pressure_narrows_the_ink() {
        let pen = ink(Tool::Pen, 4.0);
        let full = pen.mesh(
            &timed(&[fpt(0.0, 0.0, 1.0), fpt(10.0, 0.0, 1.0)]),
            StrokeEnd::Live,
            DEFAULT_TOLERANCE,
        );
        let light = pen.mesh(
            &timed(&[fpt(0.0, 0.0, 0.5), fpt(10.0, 0.0, 0.5)]),
            StrokeEnd::Live,
            DEFAULT_TOLERANCE,
        );
        let height = |m: &InkMesh| {
            let (lo, hi) = bounds(m);
            hi[1] - lo[1]
        };
        assert!((height(&full) - 4.0).abs() < 1e-3, "{}", height(&full));
        assert!((height(&light) - 3.0).abs() < 1e-3, "{}", height(&light));
    }

    #[test]
    fn stored_size_overrides_force() {
        let sized = |x: f32| StrokePoint {
            size: Some(crate::PointSize { w: 6.0, h: 6.0 }),
            ..fpt(x, 0.0, 0.1)
        };
        let mesh = ink(Tool::Pen, 2.0).mesh(
            &[sized(0.0), sized(10.0)],
            StrokeEnd::Live,
            DEFAULT_TOLERANCE,
        );
        let (lo, hi) = bounds(&mesh);
        assert!((hi[1] - lo[1] - 6.0).abs() < 1e-3, "{lo:?} {hi:?}");
    }

    #[test]
    fn single_point_renders_as_dot() {
        let mesh = mono(&[fpt(5.0, 5.0, 1.0)], 2.0);
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
        let dot = mono(&[fpt(1.0, 1.0, 1.0)], 2.0);
        let mesh = mono(
            &[fpt(1.0, 1.0, 1.0), fpt(1.0, 1.0, 1.0), fpt(1.0, 1.0, 1.0)],
            2.0,
        );
        assert_eq!(mesh, dot);
    }

    #[test]
    fn empty_and_non_finite_input_is_empty() {
        assert!(mono(&[], 2.0).is_empty());
        assert!(
            mono(
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
        let mesh = mono(&pts, 4.0);
        let (_, hi) = bounds(&mesh);
        assert!(
            hi[0] > 11.5,
            "join should bulge past the corner, got {hi:?}"
        );
    }

    #[test]
    fn finer_tolerance_adds_vertices() {
        let pts = [fpt(0.0, 0.0, 1.0), fpt(10.0, 0.0, 1.0)];
        let coarse = ink(Tool::Monoline, 8.0).mesh(&pts, StrokeEnd::Complete, 1.0);
        let fine = ink(Tool::Monoline, 8.0).mesh(&pts, StrokeEnd::Complete, 0.01);
        assert!(fine.vertices.len() > coarse.vertices.len());
    }

    #[test]
    fn stroke_space_uv_runs_along_and_across() {
        let mesh = mono(&[fpt(0.0, 0.0, 1.0), fpt(10.0, 0.0, 1.0)], 2.0);
        let u_max = mesh
            .vertices
            .iter()
            .map(|v| v.uv[0])
            .fold(0.0_f32, f32::max);
        assert!(
            (u_max - 10.0).abs() < 1e-3,
            "u spans the arc length, got {u_max}"
        );
        assert!(
            mesh.vertices
                .iter()
                .all(|v| v.uv[1] == 1.0 || v.uv[1] == -1.0)
        );
        // Left and right edges lie on opposite sides of the path.
        let left = mesh
            .vertices
            .iter()
            .filter(|v| v.uv[1] < 0.0)
            .map(|v| v.pos[1])
            .sum::<f32>();
        let right = mesh
            .vertices
            .iter()
            .filter(|v| v.uv[1] > 0.0)
            .map(|v| v.pos[1])
            .sum::<f32>();
        assert!(
            left * right < 0.0,
            "sides should straddle the path: {left} {right}"
        );
        assert!(mesh.vertices.iter().all(|v| v.opacity == 1.0));
    }

    #[test]
    fn pencil_opacity_reaches_the_vertices_and_style_carries_paint() {
        let pencil = ink(Tool::Pencil, 3.0);
        let mesh = pencil.mesh(
            &timed(&[fpt(0.0, 0.0, 0.3), fpt(10.0, 0.0, 0.3)]),
            StrokeEnd::Live,
            DEFAULT_TOLERANCE,
        );
        assert!(
            mesh.vertices
                .iter()
                .all(|v| v.opacity > 0.0 && v.opacity < 1.0),
            "pressure 0.3 is translucent"
        );
        assert_eq!(mesh.style.opacity, 0.9);

        let marker = ink(Tool::Marker, 6.0);
        let style = marker.style();
        assert_eq!(
            (style.blend, style.overlap),
            (Blend::Multiply, Overlap::Discard)
        );
        assert_eq!(style.color, Rgba::BLACK);
    }

    #[test]
    fn nib_width_follows_orientation_and_has_flat_caps() {
        // Force 0.5 is the fountain pen's nominal pressure: no flex.
        let with_nib = |x: f32, angle: f32| StrokePoint {
            tilt: Some(Tilt {
                azimuth: angle,
                altitude: 0.5,
                roll: 0.0,
            }),
            ..fpt(x, 0.0, 0.5)
        };
        let half_pi = core::f32::consts::FRAC_PI_2;
        let fountain = ink(Tool::Fountain, 8.0);
        let height = |m: &InkMesh| {
            let (lo, hi) = bounds(m);
            hi[1] - lo[1]
        };
        // Nib across the motion (pointing +y on a horizontal stroke): full width.
        let across = fountain.mesh(
            &[with_nib(0.0, half_pi), with_nib(10.0, half_pi)],
            StrokeEnd::Live,
            DEFAULT_TOLERANCE,
        );
        assert!(
            (height(&across) - 8.0).abs() < 1e-3,
            "across {}",
            height(&across)
        );
        // Flat caps: the nib rectangle does not extend past the endpoints
        // beyond its thickness.
        let (lo, hi) = bounds(&across);
        assert!(lo[0] >= -0.61 && hi[0] <= 10.61, "{lo:?} {hi:?}");
        // Nib along the motion: only the nib's own thickness remains across.
        let along = fountain.mesh(
            &[with_nib(0.0, 0.0), with_nib(10.0, 0.0)],
            StrokeEnd::Live,
            DEFAULT_TOLERANCE,
        );
        assert!(
            (height(&along) - 8.0 * 0.15).abs() < 1e-3,
            "along {}",
            height(&along)
        );
        // Roll turns the nib: azimuth 0 rolled by π/2 is across again.
        let rolled = |x: f32| StrokePoint {
            tilt: Some(Tilt {
                azimuth: 0.0,
                altitude: 0.5,
                roll: half_pi,
            }),
            ..fpt(x, 0.0, 0.5)
        };
        let rolled = fountain.mesh(
            &[rolled(0.0), rolled(10.0)],
            StrokeEnd::Live,
            DEFAULT_TOLERANCE,
        );
        assert!(
            (height(&rolled) - 8.0).abs() < 1e-3,
            "rolled {}",
            height(&rolled)
        );
        // Without tilt the preset's fallback angle applies: 45°, so the
        // stroke is neither full width nor the thin floor.
        let plain = fountain.mesh(
            &[fpt(0.0, 0.0, 0.5), fpt(10.0, 0.0, 0.5)],
            StrokeEnd::Live,
            DEFAULT_TOLERANCE,
        );
        let h = height(&plain);
        assert!(h > 5.0 && h < 7.0, "fallback nib height {h}");
    }

    #[test]
    fn outline_polygon_wraps_the_ink() {
        let mono_ink = ink(Tool::Monoline, 2.0);
        let outline = mono_ink.outline(&[fpt(0.0, 0.0, 1.0), fpt(10.0, 0.0, 1.0)]);
        assert!(outline.len() > 4);
        let xs = outline.iter().map(|p| p[0]);
        let (lo, hi) = xs.fold((f32::MAX, f32::MIN), |(lo, hi), x| (lo.min(x), hi.max(x)));
        assert!(
            (lo + 1.0).abs() < 0.05 && (hi - 11.0).abs() < 0.05,
            "caps reach past the ends: {lo} {hi}"
        );
        assert!(outline.iter().all(|p| p[1].abs() <= 1.0 + 1e-3));
        // A dot is a full circle; an empty stroke has no outline.
        assert_eq!(mono_ink.outline(&[fpt(0.0, 0.0, 1.0)]).len(), 16);
        assert!(mono_ink.outline(&[]).is_empty());
        // A nib outline has four corners at the ends.
        let nib = ink(Tool::Fountain, 8.0).outline(&[fpt(0.0, 0.0, 0.5), fpt(10.0, 0.0, 0.5)]);
        assert_eq!(nib.len(), 4);
    }

    #[test]
    fn hit_test_respects_width_and_radius() {
        let mono_ink = ink(Tool::Monoline, 2.0);
        let line = [fpt(0.0, 0.0, 1.0), fpt(10.0, 0.0, 1.0)];
        // Ink is 2 wide (half = 1). 0.5 away with no radius: hit.
        assert!(mono_ink.hits(&line, 5.0, 0.5, 0.0));
        // 3 away: miss with radius 1, hit with radius 2.5.
        assert!(!mono_ink.hits(&line, 5.0, 3.0, 1.0));
        assert!(mono_ink.hits(&line, 5.0, 3.0, 2.5));
        // Beyond the endpoint along the line: distance is to the cap.
        assert!(!mono_ink.hits(&line, 13.0, 0.0, 1.0));
        assert!(mono_ink.hits(&line, 12.5, 0.0, 2.0));
        // A dot.
        assert!(mono_ink.hits(&[fpt(3.0, 3.0, 1.0)], 3.5, 3.0, 0.0));
        assert!(!mono_ink.hits(&[], 0.0, 0.0, 100.0));
    }

    fn arb_point() -> impl Strategy<Value = StrokePoint> {
        (
            -2000.0_f32..2000.0,
            -2000.0_f32..2000.0,
            0.0_f32..=1.0,
            proptest::option::of(0.1_f32..40.0),
            0_u32..10_000,
        )
            .prop_map(|(x, y, force, w, t_ms)| StrokePoint {
                size: w.map(|w| crate::PointSize { w, h: w }),
                t_ms,
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
            for tool in [Tool::Pen, Tool::Pencil, Tool::Marker, Tool::Monoline, Tool::Fountain] {
                let mesh = ink(tool, base).mesh(&pts, StrokeEnd::Complete, tol);
                prop_assert_eq!(mesh.indices.len() % 3, 0);
                prop_assert!(mesh.vertices.iter().all(|v| v.pos.iter().chain(&v.uv).all(|c| c.is_finite()) && (0.0..=1.0).contains(&v.opacity)));
                prop_assert!(mesh.indices.iter().all(|i| (*i as usize) < mesh.vertices.len()));
                prop_assert_eq!(mesh.is_empty(), pts.is_empty());
            }
        }
    }
}
