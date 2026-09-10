//! Shape recognition: snap a freehand stroke to a [`Shape`] when it was
//! drawn well enough. Deterministic geometry, no ML, shared by every
//! platform so a snap on the iPad is the same snap on the desktop.
//!
//! Pipeline: trim the pen-down blob and the draw-and-hold tail, resample by
//! arc length, find corners (ShortStraw with a turn gate), decide whether the
//! path closes, classify by corner count, then fit and verify. Every
//! threshold is relative to the stroke's size and lives in
//! [`RecognizerParams`], so a thumbnail sketch and a whiteboard-sized one
//! snap alike.
//!
//! ```
//! use pendant_core::{Shape, StrokePoint, recognize};
//!
//! let corners = [[0.0, 0.0], [100.0, 2.0], [98.0, 60.0], [1.0, 59.0], [0.0, 0.0]];
//! let points: Vec<StrokePoint> = corners
//!     .windows(2)
//!     .flat_map(|w| (0..20).map(move |i| {
//!         let t = i as f32 / 20.0;
//!         [w[0][0] + (w[1][0] - w[0][0]) * t, w[0][1] + (w[1][1] - w[0][1]) * t]
//!     }))
//!     .enumerate()
//!     .map(|(i, [x, y])| StrokePoint { x, y, force: 1.0, t_ms: i as u32 * 8, tilt: None, size: None })
//!     .collect();
//! let snapped = recognize(&points).expect("a rough rectangle");
//! assert!(matches!(snapped.shape, Shape::Rect { .. }));
//! ```

use core::f32::consts::{FRAC_PI_2, PI, TAU};

use crate::geom::{dedupe, segment_distance2};
use crate::stroke::StrokePoint;

type P = [f32; 2];

/// A recognised primitive in canvas space (x right, y down).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shape {
    Line {
        a: P,
        b: P,
    },
    /// Head at `b`.
    Arrow {
        a: P,
        b: P,
    },
    /// Full `size` (width, height) rotated by `angle` radians about
    /// `center`; 0 is axis-aligned. A square has equal sides.
    Rect {
        center: P,
        size: P,
        angle: f32,
    },
    /// Half axes `radii` rotated by `angle` radians about `center`; a
    /// circle has equal radii and angle 0.
    Ellipse {
        center: P,
        radii: P,
        angle: f32,
    },
}

/// Shaft fraction the arrow head's wings span, and their bounds in canvas
/// units, so a tiny arrow still shows a head and a huge one is not all head.
const ARROW_WING_FRACTION: f32 = 0.22;
const ARROW_WING_MIN: f32 = 8.0;
const ARROW_WING_MAX: f32 = 40.0;
/// Half-angle of the arrow head, radians (30°).
const ARROW_WING_ANGLE: f32 = PI / 6.0;
/// Outline segment length for ellipses, canvas units, and the segment count
/// it is clamped to so zoom never shows facets.
const ELLIPSE_SEGMENT: f32 = 4.0;
const ELLIPSE_MIN_SEGMENTS: usize = 24;
const ELLIPSE_MAX_SEGMENTS: usize = 256;

impl Shape {
    /// The polyline a renderer strokes for this shape: two points for a
    /// line, shaft plus two wings for an arrow (the head retraces through
    /// `b`, which the round join covers), five for a rectangle and a
    /// perimeter-scaled ring for an ellipse. Points carry full force and
    /// no stored size, so [`crate::stroke_mesh`] draws them `base_width`
    /// wide.
    pub fn outline(&self) -> Vec<StrokePoint> {
        let pts: Vec<P> = match *self {
            Self::Line { a, b } => vec![a, b],
            Self::Arrow { a, b } => {
                let shaft = dist(a, b);
                if shaft <= f32::EPSILON {
                    vec![a, b]
                } else {
                    let wing = (ARROW_WING_FRACTION * shaft).clamp(ARROW_WING_MIN, ARROW_WING_MAX);
                    let back = scale(sub(a, b), 1.0 / shaft);
                    let w1 = add(b, scale(rotate(back, ARROW_WING_ANGLE), wing));
                    let w2 = add(b, scale(rotate(back, -ARROW_WING_ANGLE), wing));
                    vec![a, b, w1, b, w2]
                }
            }
            Self::Rect {
                center,
                size,
                angle,
            } => {
                let c = rect_corners(center, size, angle);
                vec![c[0], c[1], c[2], c[3], c[0]]
            }
            Self::Ellipse {
                center,
                radii,
                angle,
            } => {
                let segments = to_count(ellipse_perimeter(radii) / ELLIPSE_SEGMENT)
                    .clamp(ELLIPSE_MIN_SEGMENTS, ELLIPSE_MAX_SEGMENTS);
                (0..=segments)
                    .map(|i| {
                        let t = TAU * to_f32(i % segments) / to_f32(segments);
                        add(
                            center,
                            rotate([radii[0] * t.cos(), radii[1] * t.sin()], angle),
                        )
                    })
                    .collect()
            }
        };
        pts.into_iter()
            .enumerate()
            .map(|(i, [x, y])| StrokePoint {
                x,
                y,
                force: 1.0,
                t_ms: u32::try_from(i).unwrap_or(u32::MAX),
                tilt: None,
                size: None,
            })
            .collect()
    }

    /// Axis-aligned bounds of the shape's outline as `(min, max)`.
    pub fn bounds(&self) -> (P, P) {
        match *self {
            Self::Line { a, b } | Self::Arrow { a, b } => (
                [a[0].min(b[0]), a[1].min(b[1])],
                [a[0].max(b[0]), a[1].max(b[1])],
            ),
            Self::Rect {
                center,
                size,
                angle,
            } => bbox(&rect_corners(center, size, angle)).unwrap_or((center, center)),
            Self::Ellipse {
                center,
                radii,
                angle,
            } => {
                let (s, c) = angle.sin_cos();
                let hx = (radii[0] * c).hypot(radii[1] * s);
                let hy = (radii[0] * s).hypot(radii[1] * c);
                (
                    [center[0] - hx, center[1] - hy],
                    [center[0] + hx, center[1] + hy],
                )
            }
        }
    }

    fn is_finite(&self) -> bool {
        let all = |vs: &[f32]| vs.iter().all(|v| v.is_finite());
        match *self {
            Self::Line { a, b } | Self::Arrow { a, b } => all(&[a[0], a[1], b[0], b[1]]),
            Self::Rect {
                center,
                size,
                angle,
            } => {
                all(&[center[0], center[1], size[0], size[1], angle])
                    && size[0] > 0.0
                    && size[1] > 0.0
            }
            Self::Ellipse {
                center,
                radii,
                angle,
            } => {
                all(&[center[0], center[1], radii[0], radii[1], angle])
                    && radii[0] > 0.0
                    && radii[1] > 0.0
            }
        }
    }
}

fn rect_corners(center: P, size: P, angle: f32) -> [P; 4] {
    let (hw, hh) = (size[0] / 2.0, size[1] / 2.0);
    [[-hw, -hh], [hw, -hh], [hw, hh], [-hw, hh]].map(|p| add(center, rotate(p, angle)))
}

/// Ramanujan's approximation.
fn ellipse_perimeter(radii: P) -> f32 {
    let (a, b) = (radii[0], radii[1]);
    PI * (3.0 * (a + b) - ((3.0 * a + b) * (a + 3.0 * b)).sqrt())
}

/// A snapped shape and how cleanly it was drawn, 0 (barely accepted) ..= 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Recognition {
    pub shape: Shape,
    pub confidence: f32,
}

/// Every threshold the recognizer uses. Lengths are canvas units unless
/// documented as a fraction of the stroke's bounding-box diagonal (`diag`)
/// or path length (`len`); angles are radians.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecognizerParams {
    /// A tail confined to this radius of its own centroid …
    pub hold_radius: f32,
    /// … for at least this long is the draw-and-hold and collapses to that
    /// centroid. Callers that judge the hold themselves (the iPad: 3 screen
    /// points) should pass their own threshold in canvas units, with some
    /// slack; the default suits zoom 1.
    pub hold_min_ms: u32,
    /// A head confined to this radius of the first point is the pen-down
    /// blob and collapses to its centroid.
    pub head_radius: f32,
    /// Strokes with a smaller `diag` are never shapes.
    pub min_diag: f32,
    /// Resample spacing is `diag / samples_per_diag` …
    pub samples_per_diag: f32,
    /// … with the point count clamped to this range.
    pub min_samples: usize,
    pub max_samples: usize,
    /// ShortStraw half-window in samples.
    pub straw_window: usize,
    /// Straws below this fraction of the median are corner candidates.
    pub straw_threshold: f32,
    /// A candidate must also turn at least this much across the window.
    pub corner_turn: f32,
    /// A run whose path/chord ratio is at most this is straight.
    pub straight_ratio: f32,
    /// Start and end within this fraction of `diag` close the path.
    pub closure: f32,
    /// Or the last quarter passes within `closure` of the start and the
    /// end lies within this fraction of `diag` of the first quarter (the
    /// pen overshot the join and retraced the head).
    pub overshoot: f32,
    /// Line: mean perpendicular residual as a fraction of `len`.
    pub line_residual: f32,
    /// Ellipse: mean `|rho - 1|` over the ring.
    pub ellipse_radial_error: f32,
    /// Ellipse: tolerance on the total turn being one full revolution.
    pub ellipse_turn_tolerance: f32,
    /// Ellipse: longest straight run as a fraction of the ring's length.
    pub ellipse_max_straight: f32,
    /// Ellipse: a corner turning at least this much rules the ring out.
    pub ellipse_soft_corner: f32,
    /// Radii within this fraction of each other make a circle.
    pub circle_tolerance: f32,
    /// Rect: shortest side as a fraction of `diag`.
    pub rect_min_side: f32,
    /// Rect: corner angle deviation from a right angle.
    pub rect_corner_tolerance: f32,
    /// Rect: opposite sides' deviation from parallel.
    pub rect_parallel_tolerance: f32,
    /// Rect: shortest/longest of opposite sides.
    pub rect_opposite_ratio: f32,
    /// Rect: mean distance from the ring to the fitted sides, as a fraction
    /// of `diag`.
    pub rect_residual: f32,
    /// Rect and ellipse orientation within this of an axis snaps to it.
    pub axis_snap: f32,
    /// Sides within this fraction of each other make a square.
    pub square_tolerance: f32,
    /// Arrow: head wing length as a fraction of the shaft.
    pub arrow_leg_min: f32,
    pub arrow_leg_max: f32,
    /// Arrow: head wing angle off the reversed shaft.
    pub arrow_leg_angle_min: f32,
    pub arrow_leg_angle_max: f32,
    /// Below this confidence the stroke stays freehand.
    pub min_confidence: f32,
}

impl Default for RecognizerParams {
    fn default() -> Self {
        Self {
            hold_radius: 4.5,
            hold_min_ms: 120,
            head_radius: 1.5,
            min_diag: 20.0,
            samples_per_diag: 40.0,
            min_samples: 16,
            max_samples: 256,
            straw_window: 3,
            straw_threshold: 0.95,
            corner_turn: 35.0_f32.to_radians(),
            straight_ratio: 1.05,
            closure: 0.40,
            overshoot: 0.06,
            line_residual: 0.03,
            ellipse_radial_error: 0.10,
            ellipse_turn_tolerance: 0.5,
            ellipse_max_straight: 0.30,
            ellipse_soft_corner: 70.0_f32.to_radians(),
            circle_tolerance: 0.10,
            rect_min_side: 0.15,
            rect_corner_tolerance: 20.0_f32.to_radians(),
            rect_parallel_tolerance: 15.0_f32.to_radians(),
            rect_opposite_ratio: 0.75,
            rect_residual: 0.05,
            axis_snap: 8.0_f32.to_radians(),
            square_tolerance: 0.10,
            arrow_leg_min: 0.10,
            arrow_leg_max: 0.35,
            arrow_leg_angle_min: 20.0_f32.to_radians(),
            arrow_leg_angle_max: 70.0_f32.to_radians(),
            min_confidence: 0.0,
        }
    }
}

/// Recognise `points` (a modelled stroke, typically
/// [`crate::BrushModeler::points`] at hold time) with the default
/// parameters. `None` when the stroke is not a clean enough shape. Callers
/// with their own hold threshold should use [`recognize_with`] and set
/// [`RecognizerParams::hold_radius`].
pub fn recognize(points: &[StrokePoint]) -> Option<Recognition> {
    recognize_with(points, &RecognizerParams::default())
}

/// [`recognize`] with explicit thresholds.
pub fn recognize_with(points: &[StrokePoint], params: &RecognizerParams) -> Option<Recognition> {
    let clean = dedupe(points);
    let (Some(pen_down), Some(pen_now)) = (clean.first(), clean.last()) else {
        return None;
    };
    let (pen_down, pen_now) = ([pen_down.x, pen_down.y], [pen_now.x, pen_now.y]);
    let trimmed = trim_holds(&clean, params);
    let pts: Vec<P> = trimmed.iter().map(|p| [p.x, p.y]).collect();
    let (lo, hi) = bbox(&pts)?;
    let diag = dist(lo, hi);
    if diag.is_nan() || diag < params.min_diag {
        return None;
    }
    let len = path_length(&pts);
    let spacing = diag / params.samples_per_diag;
    let n = to_count(len / spacing + 1.0).clamp(params.min_samples, params.max_samples);
    let r = resample(&pts, n, Closed::No);

    let fit = match closed_loop(&r, diag, params) {
        Some((ring, completed)) => classify_closed(&ring, diag, params)
            .or_else(|| completed.and_then(|ring| classify_closed(&ring, diag, params))),
        None => classify_open(&r, len, params),
    };
    let (shape, worst) = fit?;
    // A line runs exactly from where the pen landed to where it is held;
    // an arrow's tail does the same at whichever end it was drawn from.
    let shape = match shape {
        Shape::Line { .. } => Shape::Line {
            a: pen_down,
            b: pen_now,
        },
        Shape::Arrow { a, b } => Shape::Arrow {
            a: if dist(a, pen_down) <= dist(a, pen_now) {
                pen_down
            } else {
                pen_now
            },
            b,
        },
        other => other,
    };
    let confidence = 1.0 - worst;
    (confidence >= params.min_confidence && shape.is_finite())
        .then_some(Recognition { shape, confidence })
}

// ---- stage 1: hold trimming ----

/// Collapse the pen-down blob and the draw-and-hold tail to their centroids
/// so neither reads as a hook or a corner.
fn trim_holds(points: &[StrokePoint], params: &RecognizerParams) -> Vec<StrokePoint> {
    let tail = trim_tail(points, params.hold_radius, params.hold_min_ms);
    let mut reversed: Vec<StrokePoint> = tail.into_iter().rev().collect();
    reversed = trim_tail(&reversed, params.head_radius, 0);
    reversed.reverse();
    reversed
}

/// Replace the suffix confined to `radius` of the last point by its
/// centroid when it lasted at least `min_ms`.
fn trim_tail(points: &[StrokePoint], radius: f32, min_ms: u32) -> Vec<StrokePoint> {
    let Some(last) = points.last() else {
        return Vec::new();
    };
    // The last point may sit at the edge of the jitter cloud: measure a
    // first suffix from it, then the real one from that suffix's centroid.
    let suffix_within = |center: P| {
        points
            .iter()
            .rev()
            .take_while(|p| dist([p.x, p.y], center) <= radius)
            .count()
    };
    let rough = suffix_within([last.x, last.y]);
    let center = centroid(&points[points.len() - rough..]).unwrap_or([last.x, last.y]);
    let held = suffix_within(center);
    let start = points.len() - held;
    let Some(first_held) = points.get(start) else {
        return points.to_vec();
    };
    if held < 2 || last.t_ms.saturating_sub(first_held.t_ms) < min_ms {
        return points.to_vec();
    }
    let [x, y] = centroid(&points[start..]).unwrap_or([last.x, last.y]);
    points[..start]
        .iter()
        .copied()
        .chain([StrokePoint {
            x,
            y,
            ..*first_held
        }])
        .collect()
}

fn centroid(points: &[StrokePoint]) -> Option<P> {
    if points.is_empty() {
        return None;
    }
    let n = to_f32(points.len());
    let (sx, sy) = points
        .iter()
        .fold((0.0, 0.0), |(sx, sy), p| (sx + p.x, sy + p.y));
    Some([sx / n, sy / n])
}

// ---- stage 2: resampling ----

#[derive(Clone, Copy, PartialEq, Eq)]
enum Closed {
    No,
    Yes,
}

/// `n` points spaced evenly by arc length along `pts`. Closed input is
/// treated as a ring including the segment back to the first point, and
/// the last output point stops one step short of it.
fn resample(pts: &[P], n: usize, closed: Closed) -> Vec<P> {
    let mut segs: Vec<(P, P, f32)> = pts
        .windows(2)
        .map(|w| (w[0], w[1], dist(w[0], w[1])))
        .collect();
    if closed == Closed::Yes
        && let (Some(&first), Some(&last)) = (pts.first(), pts.last())
    {
        segs.push((last, first, dist(last, first)));
    }
    let total: f32 = segs.iter().map(|s| s.2).sum();
    let (Some(first), Some(last)) = (segs.first(), segs.last()) else {
        return pts.iter().copied().cycle().take(n).collect();
    };
    if total <= f32::EPSILON || n == 0 {
        return core::iter::repeat_n(first.0, n).collect();
    }
    let step = match closed {
        Closed::Yes => total / to_f32(n),
        Closed::No => total / to_f32(n.saturating_sub(1).max(1)),
    };
    let mut seg = 0;
    let mut seg_start = 0.0;
    (0..n)
        .map(|k| {
            let target = step * to_f32(k);
            while let Some(s) = segs.get(seg)
                && seg + 1 < segs.len()
                && seg_start + s.2 < target
            {
                seg_start += s.2;
                seg += 1;
            }
            let (a, b, l) = segs.get(seg).copied().unwrap_or(*last);
            let t = if l > 0.0 {
                ((target - seg_start) / l).clamp(0.0, 1.0)
            } else {
                0.0
            };
            lerp(a, b, t)
        })
        .collect()
}

// ---- stage 3: corners ----

/// Straw at every index: the chord across `2 * w` samples, `None` where the
/// window does not fit (open paths only).
fn straws(r: &[P], w: usize, closed: Closed) -> Vec<Option<f32>> {
    (0..r.len())
        .map(|i| match closed {
            Closed::Yes => Some(dist(cyclic(r, back(r.len(), i, w)), cyclic(r, i + w))),
            Closed::No => (i >= w && i + w < r.len()).then(|| dist(r[i - w], r[i + w])),
        })
        .collect()
}

/// Ring index `i` wrapped into `r`.
fn cyclic(r: &[P], i: usize) -> P {
    let n = r.len().max(1);
    r.get(i % n).copied().unwrap_or([0.0, 0.0])
}

/// Ring index `w` steps before `i`.
fn back(n: usize, i: usize, w: usize) -> usize {
    let n = n.max(1);
    (i + n - w % n) % n
}

/// Unsigned turn across the window centred on `i`, 0 at open ends.
fn turn_at(r: &[P], i: usize, w: usize, closed: Closed) -> f32 {
    let (before, at, after) = match closed {
        Closed::Yes => (
            cyclic(r, back(r.len(), i, w)),
            cyclic(r, i),
            cyclic(r, i + w),
        ),
        Closed::No => {
            if i < w || i + w >= r.len() {
                return 0.0;
            }
            (r[i - w], r[i], r[i + w])
        }
    };
    angle_between(sub(at, before), sub(after, at))
}

fn median(values: impl Iterator<Item = f32>) -> Option<f32> {
    let mut v: Vec<f32> = values.collect();
    v.sort_by(f32::total_cmp);
    v.get(v.len() / 2).copied()
}

/// ShortStraw corner candidates: the sharpest point of every run of straws
/// below the threshold, kept when the turn there passes the gate.
fn straw_corners(r: &[P], params: &RecognizerParams, closed: Closed) -> Vec<usize> {
    let w = params.straw_window;
    let straws = straws(r, w, closed);
    let Some(median) = median(straws.iter().flatten().copied()) else {
        return Vec::new();
    };
    let threshold = params.straw_threshold * median;
    let n = r.len();
    let below = |i: usize| {
        straws
            .get(i)
            .copied()
            .flatten()
            .is_some_and(|s| s < threshold)
    };
    // Scan from an index above the threshold so a ring's run never wraps
    // around the scan start; a ring with every straw below is featureless.
    let start = match closed {
        Closed::Yes => match (0..n).find(|&i| !below(i)) {
            Some(s) => s,
            None => return Vec::new(),
        },
        Closed::No => 0,
    };
    let mut corners = Vec::new();
    let mut k = 0;
    while k < n {
        let i = (start + k) % n;
        if !below(i) {
            k += 1;
            continue;
        }
        let run_start = k;
        while k < n && below((start + k) % n) {
            k += 1;
        }
        let best = (run_start..k).map(|k| (start + k) % n).min_by(|&a, &b| {
            let s = |i: usize| straws.get(i).copied().flatten().unwrap_or(f32::INFINITY);
            s(a).total_cmp(&s(b))
        });
        if let Some(best) = best
            && turn_at(r, best, w, closed) >= params.corner_turn
        {
            corners.push(best);
        }
    }
    corners.sort_unstable();
    corners
}

/// Path length over `r[a..=b]` divided by the chord; `INFINITY` for a
/// zero chord. Ring indices walk forward cyclically from `a` to `b`.
fn straightness(r: &[P], a: usize, b: usize, closed: Closed) -> f32 {
    let n = r.len();
    let steps = match closed {
        Closed::Yes => (b + n - a) % n,
        Closed::No => b.saturating_sub(a),
    };
    let path: f32 = (0..steps)
        .map(|k| dist(cyclic(r, a + k), cyclic(r, a + k + 1)))
        .sum();
    let chord = dist(cyclic(r, a), cyclic(r, b));
    if chord <= f32::EPSILON {
        f32::INFINITY
    } else {
        path / chord
    }
}

/// Corners of an open path: both ends plus the gated ShortStraw corners,
/// refined by splitting bent segments and merging redundant corners.
fn corners_open(r: &[P], params: &RecognizerParams) -> Vec<usize> {
    let n = r.len();
    if n < 2 {
        return (0..n).collect();
    }
    let mut corners = vec![0];
    corners.extend(
        straw_corners(r, params, Closed::No)
            .into_iter()
            .filter(|&c| c > 0 && c + 1 < n),
    );
    corners.push(n - 1);
    refine(r, corners, params, Closed::No)
}

/// Corners of a ring, refined the same way with cyclic neighbours.
fn corners_closed(ring: &[P], params: &RecognizerParams) -> Vec<usize> {
    let corners = straw_corners(ring, params, Closed::Yes);
    refine(ring, corners, params, Closed::Yes)
}

/// Iterate to a fixed point (bounded): split a segment that is not
/// straight at its sharpest gated point, drop a corner whose neighbours
/// are collinear through it, and drop corners closer than two samples.
fn refine(
    r: &[P],
    mut corners: Vec<usize>,
    params: &RecognizerParams,
    closed: Closed,
) -> Vec<usize> {
    let w = params.straw_window;
    let straws = straws(r, w, closed);
    let straw = |i: usize| straws.get(i).copied().flatten().unwrap_or(f32::INFINITY);
    let n = r.len();
    let m = |corners: &Vec<usize>| corners.len();
    for _ in 0..8 {
        let mut changed = false;

        // Split. Pairs are consecutive corners; a ring also pairs the last
        // with the first.
        let pairs = match closed {
            Closed::Yes if m(&corners) >= 2 => m(&corners),
            Closed::No if m(&corners) >= 2 => m(&corners) - 1,
            _ => 0,
        };
        let mut inserts = Vec::new();
        for i in 0..pairs {
            let (a, b) = (corners[i], corners[(i + 1) % m(&corners)]);
            if straightness(r, a, b, closed) <= params.straight_ratio {
                continue;
            }
            let steps = match closed {
                Closed::Yes => (b + n - a) % n,
                Closed::No => b.saturating_sub(a),
            };
            // A candidate's turn window must not overlap either end, or
            // it would measure that corner's turn instead of its own.
            let candidate = (w..steps.saturating_sub(w).max(w))
                .map(|k| (a + k) % n)
                .filter(|&k| turn_at(r, k, w, closed) >= params.corner_turn)
                .min_by(|&x, &y| straw(x).total_cmp(&straw(y)));
            if let Some(c) = candidate {
                inserts.push(c);
            }
        }
        if !inserts.is_empty() {
            corners.extend(inserts);
            corners.sort_unstable();
            corners.dedup();
            changed = true;
        }

        // Merge collinear.
        let mut i = 0;
        while i < m(&corners) {
            let (interior, prev, next) = match closed {
                Closed::Yes if m(&corners) >= 3 => (
                    true,
                    corners[(i + m(&corners) - 1) % m(&corners)],
                    corners[(i + 1) % m(&corners)],
                ),
                Closed::No if i > 0 && i + 1 < m(&corners) => {
                    (true, corners[i - 1], corners[i + 1])
                }
                _ => (false, 0, 0),
            };
            if interior && straightness(r, prev, next, closed) <= params.straight_ratio {
                corners.remove(i);
                changed = true;
            } else {
                i += 1;
            }
        }

        // Merge near: keep the sharper of two corners within two samples;
        // an open path's ends always win.
        let mut i = 0;
        while m(&corners) >= 2 && i < m(&corners) {
            let j = (i + 1) % m(&corners);
            if closed == Closed::No && j == 0 {
                break;
            }
            let (a, b) = (corners[i], corners[j]);
            let gap = match closed {
                Closed::Yes => (b + n - a) % n,
                Closed::No => b - a,
            };
            if gap >= 2 {
                i += 1;
                continue;
            }
            let drop_first = match closed {
                Closed::No if i == 0 => false,
                Closed::No if j + 1 == m(&corners) => true,
                _ => straw(a) > straw(b),
            };
            corners.remove(if drop_first { i } else { j });
            changed = true;
        }

        if !changed {
            break;
        }
    }
    corners
}

// ---- stage 4: closure ----

/// The ring the path traces if it closes: the path cut where its last
/// quarter comes nearest the start, resampled evenly around the loop. The
/// second ring, when the gap looks like a missing corner, closes through
/// that corner instead of a straight chord; callers try it only when the
/// chord ring fits nothing.
fn closed_loop(r: &[P], diag: f32, params: &RecognizerParams) -> Option<(Vec<P>, Option<Vec<P>>)> {
    let (first, last) = (*r.first()?, *r.last()?);
    let n = r.len();
    let gap = dist(first, last);
    let (cut, near) = r
        .iter()
        .enumerate()
        .skip(n - n / 4)
        .map(|(k, p)| (k, dist(*p, first)))
        .min_by(|a, b| a.1.total_cmp(&b.1))?;
    // Overshoot: the pen passed the start and kept tracing the head of
    // the path, so the end lies on the first quarter.
    let on_head = r
        .get(..n / 4)?
        .windows(2)
        .map(|w| segment_distance2(w[0], w[1], last).sqrt())
        .fold(f32::INFINITY, f32::min);
    let closed = gap <= params.closure * diag
        || (near <= params.closure * diag && on_head <= params.overshoot * diag);
    if !closed {
        return None;
    }
    let cut = if near < gap { cut } else { n - 1 };
    let loop_pts = r.get(..=cut)?;
    let chord = resample(loop_pts, n, Closed::Yes);
    let completed = missing_corner(loop_pts, params).map(|corner| {
        let mut with_corner = loop_pts.to_vec();
        with_corner.push(corner);
        resample(&with_corner, n, Closed::Yes)
    });
    Some((chord, completed))
}

/// Where a gap spans a corner (the pen started a little way along one side
/// and stopped a little short on the neighbouring side), a straight closing
/// chord would cut the corner diagonally. If the tangents at both ends
/// meet at a sharp angle, within reach of each end, close through that
/// intersection instead.
fn missing_corner(pts: &[P], params: &RecognizerParams) -> Option<P> {
    let w = params.straw_window;
    let n = pts.len();
    if n < 2 * w + 2 {
        return None;
    }
    let (first, last) = (*pts.first()?, *pts.last()?);
    let gap = dist(first, last);
    let d_end = sub(last, *pts.get(n - 1 - w)?);
    let d_start = sub(*pts.get(w)?, first);
    if angle_between(d_end, d_start) < params.corner_turn {
        return None;
    }
    // last + t·d_end == first - u·d_start
    let denom = cross(d_end, d_start);
    if denom.abs() <= f32::EPSILON {
        return None;
    }
    let diff = sub(first, last);
    let t = cross(diff, d_start) / denom;
    let u = cross(d_end, diff) / denom;
    let corner = add(last, scale(d_end, t));
    let reach = 1.5 * gap;
    // Forward from the end, backward from the start.
    (t >= 0.0 && u >= 0.0 && dist(last, corner) <= reach && dist(first, corner) <= reach)
        .then_some(corner)
}

// ---- stage 5: classification ----

/// Open paths: no interior corner is a line, otherwise an arrow drawn
/// either way round.
fn classify_open(r: &[P], len: f32, params: &RecognizerParams) -> Option<(Shape, f32)> {
    let corners = corners_open(r, params);
    if corners.len() <= 2 {
        return fit_line(r, params).map(|(a, b, worst)| (Shape::Line { a, b }, worst));
    }
    let forward = fit_arrow(r, &corners, len, params);
    let rev: Vec<P> = r.iter().rev().copied().collect();
    let rev_corners: Vec<usize> = corners.iter().rev().map(|&c| r.len() - 1 - c).collect();
    let backward = fit_arrow(&rev, &rev_corners, len, params);
    match (forward, backward) {
        (Some(f), Some(b)) => Some(if f.1 <= b.1 { f } else { b }),
        (f, b) => f.or(b),
    }
}

/// Rings: four corners (or five with one soft enough to drop) are a
/// rectangle; otherwise an ellipse if the ring is smooth enough. A
/// rectangle that fits wins over an ellipse.
fn classify_closed(ring: &[P], diag: f32, params: &RecognizerParams) -> Option<(Shape, f32)> {
    let mut corners = corners_closed(ring, params);
    let rect = match corners.len() {
        4 => fit_rect(ring, &corners, diag, params),
        5 => {
            let softest = (0..5).min_by(|&i, &j| {
                polygon_turn(ring, &corners, i).total_cmp(&polygon_turn(ring, &corners, j))
            });
            match softest {
                Some(i) if polygon_turn(ring, &corners, i) < 50.0_f32.to_radians() => {
                    corners.remove(i);
                    fit_rect(ring, &corners, diag, params)
                }
                _ => None,
            }
        }
        _ => None,
    };
    rect.or_else(|| fit_ellipse(ring, &corners, diag, params))
}

/// Turn of the corner polygon at corner `i` (cyclic).
fn polygon_turn(ring: &[P], corners: &[usize], i: usize) -> f32 {
    let m = corners.len();
    if m < 3 {
        return 0.0;
    }
    let at = |k: usize| cyclic(ring, corners[k % m]);
    let (prev, here, next) = (at(i + m - 1), at(i), at(i + 1));
    angle_between(sub(here, prev), sub(next, here))
}

// ---- stage 6: fits ----

/// Mean and principal direction (radians) of a point set.
fn pca(pts: &[P]) -> Option<(P, f32)> {
    if pts.is_empty() {
        return None;
    }
    let n = to_f32(pts.len());
    let mean = scale(pts.iter().fold([0.0, 0.0], |acc, p| add(acc, *p)), 1.0 / n);
    let (sxx, syy, sxy) = pts.iter().fold((0.0, 0.0, 0.0), |(xx, yy, xy), p| {
        let d = sub(*p, mean);
        (xx + d[0] * d[0], yy + d[1] * d[1], xy + d[0] * d[1])
    });
    if sxx + syy <= f32::EPSILON {
        return None;
    }
    Some((mean, 0.5 * (2.0 * sxy).atan2(sxx - syy)))
}

/// Straight-line fit of `pts`: endpoints projected onto the principal
/// axis and the worst test ratio.
fn fit_line(pts: &[P], params: &RecognizerParams) -> Option<(P, P, f32)> {
    let (first, last) = (*pts.first()?, *pts.last()?);
    let (mean, angle) = pca(pts)?;
    let d = [angle.cos(), angle.sin()];
    let normal = [-d[1], d[0]];
    let residual = pts
        .iter()
        .map(|p| dot(sub(*p, mean), normal).abs())
        .sum::<f32>()
        / to_f32(pts.len());
    let len = path_length(pts);
    let chord = dist(first, last);
    if chord <= f32::EPSILON || len <= f32::EPSILON {
        return None;
    }
    let project = |p: P| add(mean, scale(d, dot(sub(p, mean), d)));
    let worst = (residual / (params.line_residual * len))
        .max((len / chord - 1.0) / (params.straight_ratio - 1.0));
    (worst <= 1.0).then(|| (project(first), project(last), worst))
}

/// Arrow with the tip at the first interior corner: the shaft before it is
/// a line, and exactly two wings of plausible length and angle fan out
/// from the tip on opposite sides (a wing may retrace through the tip).
fn fit_arrow(
    r: &[P],
    corners: &[usize],
    _len: f32,
    params: &RecognizerParams,
) -> Option<(Shape, f32)> {
    let tip = *corners.get(1)?;
    let (a, b, mut worst) = fit_line(r.get(..=tip)?, params)?;
    let shaft = dist(a, b);
    if shaft <= f32::EPSILON {
        return None;
    }
    let back = scale(sub(a, b), 1.0 / shaft);
    let wings: Vec<P> = corners
        .get(2..)?
        .iter()
        .filter_map(|&c| r.get(c).copied())
        .filter(|w| dist(*w, b) >= params.arrow_leg_min * shaft)
        .collect();
    let [w1, w2] = <[P; 2]>::try_from(wings).ok()?;
    let mut sides = [0.0, 0.0];
    for (k, w) in [w1, w2].into_iter().enumerate() {
        let v = sub(w, b);
        let frac = norm(v) / shaft;
        let angle = angle_between(back, v);
        if !(params.arrow_leg_min..=params.arrow_leg_max).contains(&frac)
            || !(params.arrow_leg_angle_min..=params.arrow_leg_angle_max).contains(&angle)
        {
            return None;
        }
        let leg_mid = (params.arrow_leg_min + params.arrow_leg_max) / 2.0;
        let leg_half = (params.arrow_leg_max - params.arrow_leg_min) / 2.0;
        let ang_mid = (params.arrow_leg_angle_min + params.arrow_leg_angle_max) / 2.0;
        let ang_half = (params.arrow_leg_angle_max - params.arrow_leg_angle_min) / 2.0;
        worst = worst
            .max((frac - leg_mid).abs() / leg_half)
            .max((angle - ang_mid).abs() / ang_half);
        sides[k] = cross(back, v);
    }
    if sides[0] * sides[1] >= 0.0 {
        return None;
    }
    Some((Shape::Arrow { a, b }, worst))
}

/// Rectangle through four ring corners.
fn fit_rect(
    ring: &[P],
    corners: &[usize],
    diag: f32,
    params: &RecognizerParams,
) -> Option<(Shape, f32)> {
    let v: Vec<P> = corners
        .iter()
        .map(|&i| ring.get(i).copied())
        .collect::<Option<_>>()?;
    let v: [P; 4] = v.try_into().ok()?;
    let sides = [0, 1, 2, 3].map(|i| sub(v[(i + 1) % 4], v[i]));
    let lens = sides.map(norm);
    if lens.iter().any(|&l| l < params.rect_min_side * diag) {
        return None;
    }
    let corner_dev = (0..4)
        .map(|i| (angle_between(neg(sides[(i + 3) % 4]), sides[i]) - FRAC_PI_2).abs())
        .fold(0.0, f32::max);
    let opposite = [(0, 2), (1, 3)];
    let parallel_dev = opposite
        .map(|(i, j)| (PI - angle_between(sides[i], sides[j])).abs())
        .into_iter()
        .fold(0.0, f32::max);
    let ratio_dev = opposite
        .map(|(i, j)| 1.0 - lens[i].min(lens[j]) / lens[i].max(lens[j]))
        .into_iter()
        .fold(0.0, f32::max);

    // Orientation modulo 90°: the length-weighted mean of 4× each side's
    // heading, which every side of a rectangle agrees on.
    let (s, c) = sides.iter().zip(lens).fold((0.0, 0.0), |(s, c), (v, l)| {
        let a = 4.0 * v[1].atan2(v[0]);
        (s + l * a.sin(), c + l * a.cos())
    });
    let mut angle = 0.25 * s.atan2(c);
    if angle.abs() <= params.axis_snap {
        angle = 0.0;
    }
    let frame: Vec<P> = v.iter().map(|p| rotate(*p, -angle)).collect();
    let (lo, hi) = bbox(&frame)?;
    let mut size = sub(hi, lo);
    let center = rotate(mid(lo, hi), angle);
    if (size[0] - size[1]).abs() <= params.square_tolerance * size[0].max(size[1]) {
        let s = (size[0] + size[1]) / 2.0;
        size = [s, s];
    }
    let fitted = rect_corners(center, size, angle);
    let residual = ring
        .iter()
        .map(|p| {
            (0..4)
                .map(|i| segment_distance2(fitted[i], fitted[(i + 1) % 4], *p))
                .fold(f32::INFINITY, f32::min)
                .sqrt()
        })
        .sum::<f32>()
        / to_f32(ring.len());

    let worst = (corner_dev / params.rect_corner_tolerance)
        .max(parallel_dev / params.rect_parallel_tolerance)
        .max(ratio_dev / (1.0 - params.rect_opposite_ratio))
        .max(residual / (params.rect_residual * diag));
    (worst <= 1.0).then_some((
        Shape::Rect {
            center,
            size,
            angle,
        },
        worst,
    ))
}

/// Ellipse from the ring's principal frame and extents.
fn fit_ellipse(
    ring: &[P],
    corners: &[usize],
    diag: f32,
    params: &RecognizerParams,
) -> Option<(Shape, f32)> {
    if corners.len() > 2
        || corners.iter().any(|&i| {
            turn_at(ring, i, params.straw_window, Closed::Yes) >= params.ellipse_soft_corner
        })
    {
        return None;
    }
    let (mean, mut angle) = pca(ring)?;
    let frame: Vec<P> = ring.iter().map(|p| rotate(sub(*p, mean), -angle)).collect();
    let (lo, hi) = bbox(&frame)?;
    let mut radii = scale(sub(hi, lo), 0.5);
    if radii[0] < 0.05 * diag || radii[1] < 0.05 * diag {
        return None;
    }
    let c = mid(lo, hi);
    let center = add(mean, rotate(c, angle));
    let radial = frame
        .iter()
        .map(|p| {
            let rho =
                (((p[0] - c[0]) / radii[0]).powi(2) + ((p[1] - c[1]) / radii[1]).powi(2)).sqrt();
            (rho - 1.0).abs()
        })
        .sum::<f32>()
        / to_f32(frame.len());
    let turn_dev = (total_turn(ring).abs() - TAU).abs();
    let straight = longest_straight_run(ring);

    if (radii[0] - radii[1]).abs() <= params.circle_tolerance * radii[0].max(radii[1]) {
        let r = (radii[0] + radii[1]) / 2.0;
        radii = [r, r];
        angle = 0.0;
    } else if angle.abs() <= params.axis_snap {
        angle = 0.0;
    } else if FRAC_PI_2 - angle.abs() <= params.axis_snap {
        radii = [radii[1], radii[0]];
        angle = 0.0;
    }

    let worst = (radial / params.ellipse_radial_error)
        .max(turn_dev / params.ellipse_turn_tolerance)
        .max(straight / params.ellipse_max_straight);
    (worst <= 1.0).then_some((
        Shape::Ellipse {
            center,
            radii,
            angle,
        },
        worst,
    ))
}

/// Signed total turn around a ring, ±2π for a simple loop.
fn total_turn(ring: &[P]) -> f32 {
    let n = ring.len();
    (0..n)
        .map(|i| {
            let (a, b, c) = (cyclic(ring, i), cyclic(ring, i + 1), cyclic(ring, i + 2));
            signed_angle(sub(b, a), sub(c, b))
        })
        .sum()
}

/// Sagitta ratio below which a chord's run counts as straight, and the
/// absolute floor that absorbs hand jitter.
const STRAIGHT_SAGITTA: f32 = 0.025;
const STRAIGHT_SAGITTA_FLOOR: f32 = 1.0;

/// Longest run of the ring whose midpoint stays within the sagitta bound
/// of its chord, as a fraction of the ring's length.
fn longest_straight_run(ring: &[P]) -> f32 {
    let n = ring.len();
    if n < 4 {
        return 0.0;
    }
    let step = (0..n)
        .map(|i| dist(cyclic(ring, i), cyclic(ring, i + 1)))
        .sum::<f32>()
        / to_f32(n);
    let mut best = 0;
    for i in 0..n {
        let mut k = 2;
        while k < n {
            let (a, b) = (cyclic(ring, i), cyclic(ring, i + k));
            let chord = dist(a, b);
            let bound = (STRAIGHT_SAGITTA * chord).max(STRAIGHT_SAGITTA_FLOOR);
            let sag = [k / 4, k / 2, 3 * k / 4]
                .into_iter()
                .map(|j| segment_distance2(a, b, cyclic(ring, i + j)).sqrt())
                .fold(0.0, f32::max);
            if sag > bound {
                break;
            }
            k += 1;
        }
        best = best.max(k - 1);
    }
    to_f32(best) * step / (to_f32(n) * step).max(f32::EPSILON)
}

// ---- vector helpers ----

fn add(a: P, b: P) -> P {
    [a[0] + b[0], a[1] + b[1]]
}

fn sub(a: P, b: P) -> P {
    [a[0] - b[0], a[1] - b[1]]
}

fn neg(a: P) -> P {
    [-a[0], -a[1]]
}

fn scale(a: P, s: f32) -> P {
    [a[0] * s, a[1] * s]
}

fn dot(a: P, b: P) -> f32 {
    a[0] * b[0] + a[1] * b[1]
}

fn cross(a: P, b: P) -> f32 {
    a[0] * b[1] - a[1] * b[0]
}

fn norm(a: P) -> f32 {
    a[0].hypot(a[1])
}

fn dist(a: P, b: P) -> f32 {
    norm(sub(a, b))
}

fn lerp(a: P, b: P, t: f32) -> P {
    add(a, scale(sub(b, a), t))
}

fn mid(a: P, b: P) -> P {
    lerp(a, b, 0.5)
}

fn rotate(p: P, angle: f32) -> P {
    let (s, c) = angle.sin_cos();
    [p[0] * c - p[1] * s, p[0] * s + p[1] * c]
}

/// Unsigned angle between two vectors, 0..=π.
fn angle_between(a: P, b: P) -> f32 {
    signed_angle(a, b).abs()
}

fn signed_angle(a: P, b: P) -> f32 {
    cross(a, b).atan2(dot(a, b))
}

fn bbox(pts: &[P]) -> Option<(P, P)> {
    pts.iter().fold(None, |acc, p| {
        let (lo, hi) = acc.unwrap_or((*p, *p));
        Some((
            [lo[0].min(p[0]), lo[1].min(p[1])],
            [hi[0].max(p[0]), hi[1].max(p[1])],
        ))
    })
}

fn path_length(pts: &[P]) -> f32 {
    pts.windows(2).map(|w| dist(w[0], w[1])).sum()
}

/// Counts here are at most a few hundred and exact in f32; `usize` has no
/// `From` into `f32`.
fn to_f32(n: usize) -> f32 {
    n as f32 // ast-grep-ignore: no-as-cast
}

/// Float-to-int has no `TryFrom`; `as` saturates and maps NaN to 0, which
/// is the clamp a sample count wants.
fn to_count(v: f32) -> usize {
    v.max(0.0).round() as usize // ast-grep-ignore: no-as-cast
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brush::{BrushModeler, RawSample};
    use crate::stroke::Tool;
    use proptest::prelude::*;

    // ---- generators ----

    /// Deterministic noise so a failure reproduces.
    struct Lcg(u64);

    impl Lcg {
        fn new(seed: u64) -> Self {
            Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
        }

        fn unit(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            f32::from(u16::try_from(self.0 >> 48).unwrap_or(0)) / 65536.0
        }

        fn jitter(&mut self, amp: f32) -> f32 {
            (self.unit() * 2.0 - 1.0) * amp
        }
    }

    const SAMPLE_MS: f64 = 8.0;

    /// Pen samples every `step` units along `path` with `jitter`, at 125 Hz.
    fn trace(path: &[P], step: f32, jitter: f32, rng: &mut Lcg) -> Vec<RawSample> {
        let n = to_count(path_length(path) / step).max(2);
        resample(path, n, Closed::No)
            .into_iter()
            .enumerate()
            .map(|(i, [x, y])| RawSample {
                x: x + rng.jitter(jitter),
                y: y + rng.jitter(jitter),
                force: 0.7,
                t_ms: 1000.0 + i as f64 * SAMPLE_MS,
                tilt: None,
            })
            .collect()
    }

    /// Append a still hold of `ms` at the pen's last position with 2 pt jitter.
    fn with_hold(mut samples: Vec<RawSample>, ms: f64, rng: &mut Lcg) -> Vec<RawSample> {
        let Some(last) = samples.last().copied() else {
            return samples;
        };
        let count = (ms / SAMPLE_MS) as usize;
        samples.extend((1..=count).map(|i| RawSample {
            x: last.x + rng.jitter(2.0),
            y: last.y + rng.jitter(2.0),
            t_ms: last.t_ms + i as f64 * SAMPLE_MS,
            ..last
        }));
        samples
    }

    /// Reverse the drawing direction, keeping timestamps increasing.
    fn reversed(samples: &[RawSample]) -> Vec<RawSample> {
        samples
            .iter()
            .rev()
            .enumerate()
            .map(|(i, s)| RawSample {
                t_ms: 1000.0 + i as f64 * SAMPLE_MS,
                ..*s
            })
            .collect()
    }

    /// The modelled stroke as the app sees it at hold time.
    fn model(samples: &[RawSample]) -> Vec<StrokePoint> {
        let mut m = BrushModeler::new(Tool::Pen, 4.0);
        for &s in samples {
            m.push(s);
        }
        m.points().to_vec()
    }

    fn transform(path: &[P], angle: f32, offset: P) -> Vec<P> {
        path.iter()
            .map(|p| add(rotate(*p, angle), offset))
            .collect()
    }

    /// Walk a ring from `start` (fraction of its perimeter) once around
    /// plus `overshoot` of another turn.
    fn loop_path(ring: &[P], start: f32, overshoot: f32) -> Vec<P> {
        let dense = resample(ring, 400, Closed::Yes);
        let first = (start * 400.0) as usize;
        let count = ((1.0 + overshoot) * 400.0) as usize + 1;
        (0..count).map(|i| dense[(first + i) % 400]).collect()
    }

    fn rect_ring(w: f32, h: f32) -> Vec<P> {
        rect_corners([0.0, 0.0], [w, h], 0.0).to_vec()
    }

    fn ellipse_ring(a: f32, b: f32) -> Vec<P> {
        (0..200)
            .map(|i| {
                let t = TAU * i as f32 / 200.0;
                [a * t.cos(), b * t.sin()]
            })
            .collect()
    }

    /// Shaft from `a` to `b` with wings of `wing` length; `retrace` draws
    /// the second wing from the tip (`A B L1 B L2`) instead of from the
    /// first wing's end (`A B L1 L2`).
    fn arrow_path(a: P, b: P, wing: f32, retrace: bool) -> Vec<P> {
        let back = scale(sub(a, b), 1.0 / dist(a, b));
        let l1 = add(b, scale(rotate(back, 0.6), wing));
        let l2 = add(b, scale(rotate(back, -0.6), wing));
        if retrace {
            vec![a, b, l1, b, l2]
        } else {
            vec![a, b, l1, l2]
        }
    }

    fn drawn(path: &[P], jitter: f32, seed: u64) -> Vec<StrokePoint> {
        let mut rng = Lcg::new(seed);
        let samples = with_hold(trace(path, 2.0, jitter, &mut rng), 500.0, &mut rng);
        model(&samples)
    }

    fn drawn_reversed(path: &[P], jitter: f32, seed: u64) -> Vec<StrokePoint> {
        let mut rng = Lcg::new(seed);
        let samples = reversed(&trace(path, 2.0, jitter, &mut rng));
        model(&with_hold(samples, 500.0, &mut rng))
    }

    fn near(a: P, b: P, tol: f32) -> bool {
        dist(a, b) <= tol
    }

    /// Angle difference modulo `period`, folded into ±period/2.
    fn angle_diff(a: f32, b: f32, period: f32) -> f32 {
        let d = (a - b).rem_euclid(period);
        d.min(period - d)
    }

    fn expect_rect(rec: Option<Recognition>, center: P, size: P, angle: f32) -> Recognition {
        let rec = rec.expect("recognised");
        match rec.shape {
            Shape::Rect {
                center: c,
                size: s,
                angle: a,
            } => {
                assert!(near(c, center, 4.0), "center {c:?} vs {center:?}");
                assert!(near(s, size, 8.0), "size {s:?} vs {size:?}");
                assert!(
                    angle_diff(a, angle, FRAC_PI_2) <= 3.0_f32.to_radians(),
                    "angle {} vs {}",
                    a.to_degrees(),
                    angle.to_degrees()
                );
            }
            other => panic!("expected a rect, got {other:?}"),
        }
        rec
    }

    fn expect_ellipse(rec: Option<Recognition>, center: P, radii: P) -> Recognition {
        let rec = rec.expect("recognised");
        match rec.shape {
            Shape::Ellipse {
                center: c,
                radii: r,
                ..
            } => {
                assert!(near(c, center, 4.0), "center {c:?} vs {center:?}");
                assert!(
                    (r[0] - radii[0]).abs() <= 0.05 * radii[0]
                        && (r[1] - radii[1]).abs() <= 0.05 * radii[1],
                    "radii {r:?} vs {radii:?}"
                );
            }
            other => panic!("expected an ellipse, got {other:?}"),
        }
        rec
    }

    // ---- positives ----

    #[test]
    fn axis_rect_snaps_to_angle_zero() {
        let path = loop_path(&rect_ring(100.0, 60.0), 0.0, 0.0);
        let rec = expect_rect(
            recognize(&drawn(&path, 1.5, 1)),
            [0.0, 0.0],
            [100.0, 60.0],
            0.0,
        );
        assert!(rec.confidence > 0.25);
        match rec.shape {
            Shape::Rect { angle, .. } => assert_eq!(angle, 0.0),
            _ => unreachable!(),
        }
    }

    #[test]
    fn slightly_rotated_rect_snaps_to_axis() {
        let path = transform(
            &loop_path(&rect_ring(100.0, 60.0), 0.0, 0.0),
            5.0_f32.to_radians(),
            [300.0, 200.0],
        );
        let rec = expect_rect(
            recognize(&drawn(&path, 1.0, 2)),
            [300.0, 200.0],
            [100.0, 60.0],
            0.0,
        );
        assert!(matches!(rec.shape, Shape::Rect { angle, .. } if angle == 0.0));
    }

    #[test]
    fn rotated_rect_keeps_its_angle() {
        let angle = 20.0_f32.to_radians();
        let path = transform(
            &loop_path(&rect_ring(100.0, 60.0), 0.0, 0.0),
            angle,
            [50.0, 50.0],
        );
        expect_rect(
            recognize(&drawn(&path, 1.0, 3)),
            [50.0, 50.0],
            [100.0, 60.0],
            angle,
        );
    }

    #[test]
    fn square_gets_equal_sides() {
        let path = loop_path(&rect_ring(80.0, 76.0), 0.0, 0.0);
        let rec = expect_rect(
            recognize(&drawn(&path, 1.0, 4)),
            [0.0, 0.0],
            [78.0, 78.0],
            0.0,
        );
        assert!(matches!(rec.shape, Shape::Rect { size, .. } if size[0] == size[1]));
    }

    #[test]
    fn rect_started_mid_side_and_overshooting() {
        let path = loop_path(&rect_ring(100.0, 60.0), 0.125, 0.15);
        expect_rect(
            recognize(&drawn(&path, 1.0, 5)),
            [0.0, 0.0],
            [100.0, 60.0],
            0.0,
        );
    }

    #[test]
    fn rect_started_at_a_corner() {
        let path = loop_path(&rect_ring(100.0, 60.0), 0.3125, 0.0);
        expect_rect(
            recognize(&drawn(&path, 1.0, 6)),
            [0.0, 0.0],
            [100.0, 60.0],
            0.0,
        );
    }

    #[test]
    fn ellipse_axes_within_five_percent() {
        let path = loop_path(&ellipse_ring(50.0, 30.0), 0.0, 0.0);
        let rec = expect_ellipse(recognize(&drawn(&path, 1.0, 7)), [0.0, 0.0], [50.0, 30.0]);
        assert!(matches!(rec.shape, Shape::Ellipse { angle, .. } if angle == 0.0));
    }

    #[test]
    fn tall_ellipse_reports_axes_upright() {
        let path = loop_path(&ellipse_ring(30.0, 50.0), 0.2, 0.05);
        let rec = expect_ellipse(recognize(&drawn(&path, 1.0, 8)), [0.0, 0.0], [30.0, 50.0]);
        assert!(matches!(rec.shape, Shape::Ellipse { angle, .. } if angle == 0.0));
    }

    #[test]
    fn rotated_ellipse_keeps_its_angle() {
        let angle = 30.0_f32.to_radians();
        let path = transform(
            &loop_path(&ellipse_ring(60.0, 30.0), 0.0, 0.0),
            angle,
            [0.0, 0.0],
        );
        let rec = expect_ellipse(recognize(&drawn(&path, 0.8, 9)), [0.0, 0.0], [60.0, 30.0]);
        match rec.shape {
            Shape::Ellipse { angle: a, .. } => {
                assert!(
                    angle_diff(a, angle, PI) <= 4.0_f32.to_radians(),
                    "{}",
                    a.to_degrees()
                )
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn circle_gets_equal_radii() {
        let path = loop_path(&ellipse_ring(40.0, 42.0), 0.6, 0.1);
        let rec = expect_ellipse(recognize(&drawn(&path, 1.0, 10)), [0.0, 0.0], [41.0, 41.0]);
        assert!(
            matches!(rec.shape, Shape::Ellipse { radii, angle, .. } if radii[0] == radii[1] && angle == 0.0)
        );
    }

    #[test]
    fn line_endpoints_within_three_points() {
        let path = [[0.0, 0.0], [120.0, 40.0]];
        let rec = recognize(&drawn(&path, 1.0, 11)).expect("line");
        match rec.shape {
            Shape::Line { a, b } => {
                assert!(near(a, [0.0, 0.0], 3.0), "{a:?}");
                assert!(near(b, [120.0, 40.0], 3.0), "{b:?}");
            }
            other => panic!("expected a line, got {other:?}"),
        }
    }

    #[test]
    fn arrows_in_every_drawing_order() {
        let (a, b) = ([0.0, 0.0], [150.0, 30.0]);
        let retrace = arrow_path(a, b, 30.0, true);
        let open = arrow_path(a, b, 30.0, false);
        let head_first: Vec<P> = retrace.iter().rev().copied().collect();
        for (name, path) in [
            ("retrace", retrace),
            ("open", open),
            ("head first", head_first),
        ] {
            let rec = recognize(&drawn(&path, 1.0, 12)).unwrap_or_else(|| panic!("{name}"));
            match rec.shape {
                Shape::Arrow { a: ra, b: rb } => {
                    assert!(near(ra, a, 5.0), "{name}: a {ra:?}");
                    assert!(near(rb, b, 5.0), "{name}: b {rb:?}");
                }
                other => panic!("{name}: expected an arrow, got {other:?}"),
            }
        }
    }

    #[test]
    fn reversed_input_gives_the_same_shape() {
        let rect = loop_path(&rect_ring(100.0, 60.0), 0.0, 0.0);
        expect_rect(
            recognize(&drawn_reversed(&rect, 1.0, 13)),
            [0.0, 0.0],
            [100.0, 60.0],
            0.0,
        );
        let ellipse = loop_path(&ellipse_ring(50.0, 30.0), 0.0, 0.0);
        expect_ellipse(
            recognize(&drawn_reversed(&ellipse, 1.0, 14)),
            [0.0, 0.0],
            [50.0, 30.0],
        );
        let line = [[0.0, 0.0], [120.0, 40.0]];
        match recognize(&drawn_reversed(&line, 1.0, 15))
            .expect("line")
            .shape
        {
            Shape::Line { a, b } => {
                assert!(
                    near(a, [120.0, 40.0], 3.0) && near(b, [0.0, 0.0], 3.0),
                    "{a:?} {b:?}"
                )
            }
            other => panic!("{other:?}"),
        }
    }

    // ---- negatives ----

    #[test]
    fn letters_and_scribbles_stay_freehand() {
        let s_curve: Vec<P> = (0..=100)
            .map(|i| {
                let t = i as f32 / 100.0;
                [40.0 * (t * TAU).sin(), 120.0 * t]
            })
            .collect();
        let m = vec![
            [0.0, 100.0],
            [20.0, 0.0],
            [40.0, 70.0],
            [60.0, 0.0],
            [80.0, 100.0],
        ];
        let z = vec![[0.0, 0.0], [100.0, 0.0], [0.0, 80.0], [100.0, 80.0]];
        let c: Vec<P> = (0..=60)
            .map(|i| {
                let t = PI / 2.0 + PI * i as f32 / 60.0;
                [50.0 * t.cos(), 50.0 * t.sin()]
            })
            .collect();
        let semicircle: Vec<P> = (0..=60)
            .map(|i| {
                let t = PI * i as f32 / 60.0;
                [50.0 * t.cos(), -50.0 * t.sin()]
            })
            .chain([[-50.0, 0.0], [50.0, 0.0]])
            .collect();
        let triangle = loop_path(&[[0.0, 0.0], [100.0, 0.0], [50.0, 80.0]], 0.0, 0.0);
        let figure_eight: Vec<P> = (0..=200)
            .map(|i| {
                let t = TAU * i as f32 / 200.0;
                [60.0 * t.sin(), 30.0 * (2.0 * t).sin()]
            })
            .collect();
        let mut rng = Lcg::new(99);
        let scribble: Vec<P> = (0..40)
            .map(|_| [rng.jitter(60.0), rng.jitter(60.0)])
            .collect();
        for (name, path) in [
            ("S", s_curve),
            ("M", m),
            ("Z", z),
            ("C", c),
            ("semicircle", semicircle),
            ("triangle", triangle),
            ("figure eight", figure_eight),
            ("scribble", scribble),
        ] {
            let rec = recognize(&drawn(&path, 0.8, 16));
            assert!(rec.is_none(), "{name} recognised as {rec:?}");
        }
    }

    #[test]
    fn tiny_and_degenerate_input_is_none() {
        let tick = [[0.0, 0.0], [6.0, 5.0]];
        assert!(recognize(&drawn(&tick, 0.5, 17)).is_none());
        let dot = [[10.0, 10.0]];
        assert!(recognize(&drawn(&dot, 0.0, 18)).is_none());
        assert!(recognize(&[]).is_none());
        let nan = StrokePoint {
            x: f32::NAN,
            y: f32::NAN,
            force: 1.0,
            t_ms: 0,
            tilt: None,
            size: None,
        };
        assert!(recognize(&[nan; 30]).is_none());
    }

    // ---- stages ----

    fn pt(x: f32, y: f32, t_ms: u32) -> StrokePoint {
        StrokePoint {
            x,
            y,
            force: 1.0,
            t_ms,
            tilt: None,
            size: None,
        }
    }

    #[test]
    fn hold_trim_collapses_a_slow_tail_but_keeps_a_fast_corner() {
        let params = RecognizerParams::default();
        // 40 units of line, then 300 ms of jitter within 2 pt.
        let mut held: Vec<StrokePoint> = (0..20).map(|i| pt(i as f32 * 2.0, 0.0, i * 8)).collect();
        held.extend((0..30).map(|i| pt(38.0 + (i % 3) as f32, (i % 2) as f32, 160 + i * 10)));
        let trimmed = trim_holds(&held, &params);
        // The whole cloud and the slow line points inside the radius go;
        // the run before them is untouched.
        assert!(trimmed.len() <= 20 && trimmed.len() >= 17, "{trimmed:?}");
        assert_eq!(trimmed[..trimmed.len() - 1], held[..trimmed.len() - 1]);
        let end = trimmed[trimmed.len() - 1];
        assert!((end.x - 39.0).abs() < 0.5 && end.y.abs() < 0.6, "{end:?}");

        // A hairpin drawn in 16 ms within the same radius survives intact.
        let fast = vec![
            pt(0.0, 0.0, 0),
            pt(20.0, 0.0, 40),
            pt(20.0, 2.0, 48),
            pt(18.0, 2.0, 56),
        ];
        assert_eq!(trim_holds(&fast, &params), fast);
    }

    #[test]
    fn resampling_is_uniform() {
        let path = [[0.0, 0.0], [10.0, 0.0], [10.0, 30.0], [-5.0, 30.0]];
        let r = resample(&path, 12, Closed::No);
        assert_eq!(r.len(), 12);
        assert_eq!(r[0], [0.0, 0.0]);
        assert_eq!(r[11], [-5.0, 30.0]);
        let steps: Vec<f32> = r.windows(2).map(|w| dist(w[0], w[1])).collect();
        let expect = 55.0 / 11.0;
        assert!(steps.iter().all(|s| (s - expect).abs() < 1e-3), "{steps:?}");

        let ring = resample(&rect_ring(40.0, 40.0), 16, Closed::Yes);
        assert_eq!(ring.len(), 16);
        let steps: Vec<f32> = (0..16).map(|i| dist(ring[i], ring[(i + 1) % 16])).collect();
        assert!(steps.iter().all(|s| (s - 10.0).abs() < 1e-3), "{steps:?}");
    }

    #[test]
    fn short_straw_counts_corners() {
        let params = RecognizerParams::default();
        let square = resample(&rect_ring(100.0, 100.0), 113, Closed::Yes);
        assert_eq!(corners_closed(&square, &params).len(), 4);
        let circle = resample(&ellipse_ring(50.0, 50.0), 89, Closed::Yes);
        assert_eq!(corners_closed(&circle, &params), Vec::<usize>::new());
        let line = resample(&[[0.0, 0.0], [100.0, 20.0]], 41, Closed::No);
        assert_eq!(corners_open(&line, &params), vec![0, 40]);
    }

    #[test]
    fn outlines_have_the_expected_length_and_bounds() {
        let line = Shape::Line {
            a: [0.0, 0.0],
            b: [10.0, 0.0],
        };
        assert_eq!(line.outline().len(), 2);
        let arrow = Shape::Arrow {
            a: [0.0, 0.0],
            b: [100.0, 0.0],
        };
        let out = arrow.outline();
        assert_eq!(out.len(), 5);
        assert_eq!((out[1].x, out[1].y), (out[3].x, out[3].y));
        assert!(out[2].x < 100.0 && out[4].x < 100.0 && out[2].y * out[4].y < 0.0);
        let rect = Shape::Rect {
            center: [10.0, 10.0],
            size: [20.0, 10.0],
            angle: 0.0,
        };
        assert_eq!(rect.outline().len(), 5);
        assert_eq!(rect.bounds(), ([0.0, 5.0], [20.0, 15.0]));
        let ellipse = Shape::Ellipse {
            center: [0.0, 0.0],
            radii: [50.0, 30.0],
            angle: FRAC_PI_2,
        };
        let out = ellipse.outline();
        assert!(out.len() >= 25 && out.len() <= 257);
        assert_eq!(
            (out[0].x, out[0].y),
            (out[out.len() - 1].x, out[out.len() - 1].y)
        );
        let (lo, hi) = ellipse.bounds();
        assert!(
            near(lo, [-30.0, -50.0], 1e-3) && near(hi, [30.0, 50.0], 1e-3),
            "{lo:?} {hi:?}"
        );
    }

    fn rounded_rect_ring(w: f32, h: f32, radius: f32) -> Vec<P> {
        let (hw, hh) = (w / 2.0 - radius, h / 2.0 - radius);
        let centers = [[hw, hh], [-hw, hh], [-hw, -hh], [hw, -hh]];
        let mut ring = Vec::new();
        for (k, c) in centers.iter().enumerate() {
            for i in 0..12 {
                let t = FRAC_PI_2 * (k as f32 + i as f32 / 12.0);
                ring.push([c[0] + radius * t.cos(), c[1] + radius * t.sin()]);
            }
        }
        ring
    }

    fn wobbly_circle_ring(r: f32, wobble: f32) -> Vec<P> {
        (0..200)
            .map(|i| {
                let t = TAU * i as f32 / 200.0;
                let rr =
                    r * (1.0 + wobble * (2.0 * t).sin() + 0.6 * wobble * (3.0 * t + 1.0).sin());
                [rr * t.cos(), rr * t.sin()]
            })
            .collect()
    }

    /// Open path around a ring: from `start` fraction for `turns` of the
    /// perimeter (< 1 leaves a gap, > 1 overshoots).
    fn arc_path(ring: &[P], start: f32, turns: f32) -> Vec<P> {
        let dense = resample(ring, 400, Closed::Yes);
        let first = (start * 400.0) as usize;
        let count = (turns * 400.0) as usize + 1;
        (0..count).map(|i| dense[(first + i) % 400]).collect()
    }

    /// Hand-drawn closed shapes: rounded corners, wobble, jitter, and a
    /// gap or overshoot at the join (people rarely close a loop exactly).
    #[test]
    fn rough_and_unclosed_shapes_snap() {
        let mut cases: Vec<(String, Vec<P>, f32)> = Vec::new();
        for &radius in &[4.0, 8.0, 14.0] {
            for &turns in &[0.88, 0.95, 1.0, 1.08] {
                for &jitter in &[1.0, 2.5] {
                    cases.push((
                        format!("rect r{radius} turns{turns} j{jitter}"),
                        arc_path(&rounded_rect_ring(160.0, 100.0, radius), 0.1, turns),
                        jitter,
                    ));
                }
            }
        }
        for &wobble in &[0.04, 0.08, 0.12] {
            for &turns in &[0.85, 0.92, 1.0, 1.1] {
                for &jitter in &[1.0, 2.5] {
                    cases.push((
                        format!("circle w{wobble} turns{turns} j{jitter}"),
                        arc_path(&wobbly_circle_ring(60.0, wobble), 0.3, turns),
                        jitter,
                    ));
                }
            }
        }
        let params = RecognizerParams::default();
        let mut fails = 0;
        for (i, (name, path, jitter)) in cases.iter().enumerate() {
            let pts = drawn(path, *jitter, 100 + i as u64);
            let rec = recognize(&pts);
            let got = match rec.map(|r| r.shape) {
                Some(Shape::Rect { .. }) => "rect",
                Some(Shape::Ellipse { .. }) => "ellipse",
                Some(Shape::Line { .. }) => "line",
                Some(Shape::Arrow { .. }) => "arrow",
                None => "none",
            };
            let want = if name.starts_with("rect") {
                "rect"
            } else {
                "ellipse"
            };
            if got != want {
                fails += 1;
                // stage info
                let trimmed = trim_holds(&dedupe(&pts), &params);
                let p: Vec<P> = trimmed.iter().map(|p| [p.x, p.y]).collect();
                let (lo, hi) = bbox(&p).unwrap();
                let diag = dist(lo, hi);
                let len = path_length(&p);
                let n = to_count(len / (diag / 40.0) + 1.0).clamp(16, 256);
                let r = resample(&p, n, Closed::No);
                match closed_loop(&r, diag, &params).map(|(ring, _)| ring) {
                    Some(ring) => {
                        let cc = corners_closed(&ring, &params);
                        let turns: Vec<i32> = cc
                            .iter()
                            .enumerate()
                            .map(|(i, _)| polygon_turn(&ring, &cc, i).to_degrees() as i32)
                            .collect();
                        let rect = if cc.len() == 4 {
                            fit_rect(&ring, &cc, diag, &params).map(|f| f.1)
                        } else {
                            None
                        };
                        let ell = fit_ellipse(&ring, &cc, diag, &params).map(|f| f.1);
                        eprintln!(
                            "FAIL {name}: got {got}; closed, corners {cc:?} turns {turns:?} rect worst {rect:?} ellipse worst {ell:?}"
                        );
                    }
                    None => {
                        let gap = dist(r[0], r[r.len() - 1]) / diag;
                        eprintln!("FAIL {name}: got {got}; OPEN gap {gap:.2}·diag");
                    }
                }
            }
        }
        assert_eq!(fails, 0, "{fails} of {} rough shapes rejected", cases.len());
    }

    // ---- properties ----

    fn arb_point() -> impl Strategy<Value = StrokePoint> {
        (
            -2000.0_f32..2000.0,
            -2000.0_f32..2000.0,
            0.0_f32..=1.0,
            0_u32..10_000,
        )
            .prop_map(|(x, y, force, t_ms)| StrokePoint {
                x,
                y,
                force,
                t_ms,
                tilt: None,
                size: None,
            })
    }

    fn arb_shape_path() -> impl Strategy<Value = Vec<P>> {
        prop_oneof![
            (60.0_f32..200.0, 60.0_f32..200.0, 0.0_f32..1.0).prop_map(|(w, h, start)| loop_path(
                &rect_ring(w, h),
                start,
                0.05
            )),
            // Beyond about 2.5:1 the tips of an ellipse turn sharply
            // enough across the straw window to count as corners.
            (30.0_f32..100.0, 0.4_f32..2.5, 0.0_f32..1.0).prop_map(|(a, ratio, start)| loop_path(
                &ellipse_ring(a, a * ratio),
                start,
                0.05
            )),
            (60.0_f32..200.0, -100.0_f32..100.0).prop_map(|(x, y)| vec![[0.0, 0.0], [x, y]]),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn never_panics_and_output_is_sane(
            pts in proptest::collection::vec(arb_point(), 0..80),
        ) {
            if let Some(rec) = recognize(&pts) {
                prop_assert!((0.0..=1.0).contains(&rec.confidence));
                let out = rec.shape.outline();
                prop_assert!(out.iter().all(|p| p.x.is_finite() && p.y.is_finite()));
                let expected = match rec.shape {
                    Shape::Line { .. } => 2,
                    Shape::Arrow { .. } => 5,
                    Shape::Rect { .. } => 5,
                    Shape::Ellipse { .. } => out.len(),
                };
                prop_assert_eq!(out.len(), expected);
                let (lo, hi) = rec.shape.bounds();
                prop_assert!(lo[0] <= hi[0] && lo[1] <= hi[1]);
            }
        }

        #[test]
        fn clean_shapes_are_recognised_equivariantly(
            path in arb_shape_path(),
            angle in 0.0_f32..TAU,
            scale_by in 0.6_f32..3.0,
            dx in -1000.0_f32..1000.0,
            dy in -1000.0_f32..1000.0,
            seed in 0_u64..1000,
        ) {
            let base = drawn(&transform(&path, angle, [0.0, 0.0]), 0.5, seed);
            let rec = recognize(&base);
            prop_assert!(rec.is_some(), "clean shape not recognised");
            let moved: Vec<StrokePoint> = base
                .iter()
                .map(|p| StrokePoint { x: p.x * scale_by + dx, y: p.y * scale_by + dy, ..*p })
                .collect();
            // The app passes its hold threshold in canvas units, which
            // scales with the canvas like everything else.
            let scaled = RecognizerParams {
                hold_radius: RecognizerParams::default().hold_radius * scale_by,
                ..RecognizerParams::default()
            };
            let rec2 = recognize_with(&moved, &scaled).expect("moved shape recognised");
            let (Some(a), Some(b)) = (rec, Some(rec2)) else { unreachable!() };
            prop_assert_eq!(core::mem::discriminant(&a.shape), core::mem::discriminant(&b.shape));
            let (lo, hi) = a.shape.bounds();
            let (lo2, hi2) = b.shape.bounds();
            let expect = |p: P| [p[0] * scale_by + dx, p[1] * scale_by + dy];
            // Hold and head trimming use absolute radii, so a scaled copy
            // trims a different fraction of the stroke: allow a few units
            // plus a share of the size.
            let tol = 4.0 + 0.03 * dist(lo2, hi2);
            prop_assert!(near(lo2, expect(lo), tol), "{:?} vs {:?}", lo2, expect(lo));
            prop_assert!(near(hi2, expect(hi), tol), "{:?} vs {:?}", hi2, expect(hi));
            // Deterministic.
            prop_assert_eq!(recognize(&base), rec);
        }
    }
}
