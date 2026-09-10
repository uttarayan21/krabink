//! Brush model: turns raw pen samples into stroke points that carry their
//! rendered width, so every platform draws the same ink from the same
//! input. One [`BrushModeler`] lives per stroke in progress.
//!
//! Per sample: streamline (an exponential moving average that trails the
//! pen and smooths hand jitter), a smoothed speed estimate, width from
//! force and speed, start taper. [`BrushModeler::finish`] flushes the
//! smoothing lag by landing on the last raw sample exactly and applies the
//! end taper. [`BrushModeler::predict`] runs the same maths on a scratch
//! copy for the renderer's predicted-touch tail.
//!
//! ```
//! use pendant_core::{BrushModeler, RawSample, Tool};
//!
//! let mut modeler = BrushModeler::new(Tool::Pen, 4.0);
//! for i in 0..10 {
//!     let raw = RawSample { x: i as f32 * 3.0, y: 0.0, force: 0.6, t_ms: i as f64 * 8.0, tilt: None };
//!     modeler.push(raw);
//! }
//! let points = modeler.finish();
//! assert!(points.iter().all(|p| p.size.is_some()));
//! ```

use crate::stroke::{PointSize, StrokePoint, Tilt, Tool};

/// One raw input sample, before smoothing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RawSample {
    pub x: f32,
    pub y: f32,
    /// Normalised pressure, 0..=1. Pens without pressure report 1.
    pub force: f32,
    /// Milliseconds on any monotonic clock; only differences matter.
    pub t_ms: f64,
    pub tilt: Option<Tilt>,
}

/// How a tool turns samples into ink. Constants live here so both
/// platforms agree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrushParams {
    /// Full ink width in canvas units.
    pub size: f32,
    /// How much force and speed narrow the ink: 0 (constant width) ..= 1.
    pub thinning: f32,
    /// Streamline factor: 0 follows the pen exactly, 1 never moves. The
    /// smoothed point covers `1 - streamline` of the distance to each raw
    /// sample.
    pub streamline: f32,
    /// Speed (canvas units per ms) at which speed thinning saturates.
    pub speed_ref: f32,
    /// Ink never gets narrower than this fraction of `size`.
    pub min_width: f32,
    /// Length of the lead-in taper in canvas units (0 = none).
    pub taper_start: f32,
    /// Length of the tail taper in canvas units (0 = none).
    pub taper_end: f32,
    /// Pressure below this counts as this, so a light touch still inks.
    pub min_force: f32,
    /// Smoothed samples closer than this to the previous emitted point are
    /// dropped: a slow pen at 240 Hz would otherwise emit near-duplicates.
    pub min_distance: f32,
}

/// Fraction of the width speed thinning can remove at full `thinning`.
const SPEED_THINNING: f32 = 0.5;
/// EMA factor for the speed estimate: share of each new measurement.
const SPEED_SMOOTHING: f32 = 0.5;

impl BrushParams {
    /// The tuned parameters for `tool` at `size` canvas units wide.
    pub fn for_tool(tool: Tool, size: f32) -> Self {
        let size = size.max(0.0);
        let base = Self {
            size,
            thinning: 0.0,
            streamline: 0.3,
            speed_ref: 1.5,
            min_width: 0.25,
            taper_start: 0.0,
            taper_end: 0.0,
            min_force: 0.15,
            min_distance: 0.25,
        };
        match tool {
            Tool::Pen => Self {
                thinning: 0.5,
                streamline: 0.5,
                taper_end: size * 1.5,
                ..base
            },
            Tool::Marker => Self {
                streamline: 0.35,
                min_distance: 0.5,
                ..base
            },
            Tool::Monoline => Self {
                streamline: 0.2,
                ..base
            },
            // The nib's width comes from its orientation, not from force.
            Tool::Brush => base,
        }
    }

    /// Rendered width at `force`, moving at `speed` (canvas units per ms),
    /// `arc` canvas units into the stroke.
    fn width(&self, force: f32, speed: f32, arc: f32) -> f32 {
        let force = force.clamp(self.min_force, 1.0);
        let pressure = 1.0 + (force - 1.0) * self.thinning;
        let saturation = if self.speed_ref > 0.0 {
            (speed / self.speed_ref).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let velocity = 1.0 - self.thinning * SPEED_THINNING * saturation;
        let lead_in = taper_factor(arc, self.taper_start);
        (self.size * pressure * velocity * lead_in).max(self.size * self.min_width)
    }

    fn floor(&self) -> f32 {
        self.size * self.min_width
    }
}

/// 0 at the taper's start, 1 once `distance` reaches `length`; 1 when
/// there is no taper.
fn taper_factor(distance: f32, length: f32) -> f32 {
    if length > 0.0 {
        (distance / length).clamp(0.0, 1.0)
    } else {
        1.0
    }
}

/// Everything the smoothing needs to continue from; small so
/// [`BrushModeler::predict`] can clone it per frame.
#[derive(Debug, Clone, Copy, PartialEq)]
struct State {
    /// Timestamp of the first sample; emitted `t_ms` count from here.
    origin_ms: f64,
    /// Last emitted (smoothed) position.
    prev: [f32; 2],
    last_raw: RawSample,
    /// Smoothed speed, canvas units per ms.
    speed: f32,
    /// Arc length of the smoothed path so far.
    arc: f32,
}

/// Turns one stroke's raw samples into modelled [`StrokePoint`]s. Owns the
/// points emitted so far; renderers draw [`points`](Self::points) plus a
/// [`predict`](Self::predict) tail every frame and commit
/// [`finish`](Self::finish) at pen-up.
#[derive(Debug, Clone, PartialEq)]
pub struct BrushModeler {
    tool: Tool,
    params: BrushParams,
    state: Option<State>,
    points: Vec<StrokePoint>,
    /// Arc length at each emitted point, for the end taper.
    arcs: Vec<f32>,
}

impl BrushModeler {
    /// A modeler for `tool` with the tool's tuned parameters at `size`.
    pub fn new(tool: Tool, size: f32) -> Self {
        Self::with_params(tool, BrushParams::for_tool(tool, size))
    }

    pub fn with_params(tool: Tool, params: BrushParams) -> Self {
        Self {
            tool,
            params,
            state: None,
            points: Vec::new(),
            arcs: Vec::new(),
        }
    }

    pub fn tool(&self) -> Tool {
        self.tool
    }

    pub fn params(&self) -> &BrushParams {
        &self.params
    }

    /// Points emitted so far, without the end taper.
    pub fn points(&self) -> &[StrokePoint] {
        &self.points
    }

    /// Feed one raw sample; the point it produced, if it moved far enough
    /// from the previous one to be worth emitting.
    pub fn push(&mut self, raw: RawSample) -> Option<StrokePoint> {
        let (state, point) = self.step(self.state, raw);
        self.state = Some(state);
        if let Some(p) = point {
            self.points.push(p);
            self.arcs.push(state.arc);
        }
        point
    }

    /// The points `raw` would produce if pushed now, without pushing them.
    /// For Apple's predicted touches: draw as a tail, discard next frame.
    pub fn predict(&self, raw: &[RawSample]) -> Vec<StrokePoint> {
        let mut state = self.state;
        raw.iter()
            .filter_map(|&sample| {
                let (next, point) = self.step(state, sample);
                state = Some(next);
                point
            })
            .collect()
    }

    /// The finished stroke: every emitted point, the last raw sample
    /// landed exactly (streamline always lags the pen), and the end taper.
    pub fn finish(&self) -> Vec<StrokePoint> {
        let mut points = self.points.clone();
        let mut arcs = self.arcs.clone();
        if let Some(state) = self.state
            && [state.last_raw.x, state.last_raw.y] != state.prev
        {
            let raw = state.last_raw;
            let arc = state.arc + distance(state.prev, [raw.x, raw.y]);
            points.push(self.point(&state, raw, [raw.x, raw.y], arc));
            arcs.push(arc);
        }
        self.taper_end(&mut points, &arcs);
        points
    }

    /// Narrow the tail over the last `taper_end` canvas units, or half the
    /// stroke when it is shorter than that, so a dot or a short tick keeps
    /// its width instead of vanishing into the floor.
    fn taper_end(&self, points: &mut [StrokePoint], arcs: &[f32]) {
        let Some(&total) = arcs.last() else {
            return;
        };
        let length = self.params.taper_end.min(total / 2.0);
        if length <= 0.0 {
            return;
        }
        let floor = self.params.floor();
        for (p, &arc) in points.iter_mut().zip(arcs) {
            let factor = taper_factor(total - arc, length);
            if let Some(size) = p.size.as_mut() {
                size.w = (size.w * factor).max(floor);
                size.h = (size.h * factor).max(floor);
            }
        }
    }

    /// Advance `state` by one sample. Pure: the caller decides whether to
    /// keep the new state, which is what lets `predict` share the code.
    fn step(&self, state: Option<State>, raw: RawSample) -> (State, Option<StrokePoint>) {
        let Some(prev) = state else {
            let state = State {
                origin_ms: raw.t_ms,
                prev: [raw.x, raw.y],
                last_raw: raw,
                speed: 0.0,
                arc: 0.0,
            };
            let point = self.point(&state, raw, [raw.x, raw.y], 0.0);
            return (state, Some(point));
        };

        let dt = raw.t_ms - prev.last_raw.t_ms;
        let moved_raw = distance([prev.last_raw.x, prev.last_raw.y], [raw.x, raw.y]);
        let mut state = State {
            last_raw: raw,
            ..prev
        };
        if dt > 0.0 {
            // f64 -> f32 has no trait conversion; a millisecond delta fits
            // f32 with room to spare.
            let speed = moved_raw / dt as f32; // ast-grep-ignore: no-as-cast
            state.speed += (speed - state.speed) * SPEED_SMOOTHING;
        }

        let follow = 1.0 - self.params.streamline.clamp(0.0, 1.0);
        let smoothed = [
            prev.prev[0] + (raw.x - prev.prev[0]) * follow,
            prev.prev[1] + (raw.y - prev.prev[1]) * follow,
        ];
        let moved = distance(prev.prev, smoothed);
        if moved < self.params.min_distance {
            return (state, None);
        }
        state.prev = smoothed;
        state.arc += moved;
        let point = self.point(&state, raw, smoothed, state.arc);
        (state, Some(point))
    }

    fn point(&self, state: &State, raw: RawSample, at: [f32; 2], arc: f32) -> StrokePoint {
        let width = self.params.width(raw.force, state.speed, arc);
        StrokePoint {
            x: at[0],
            y: at[1],
            force: raw.force.clamp(0.0, 1.0),
            t_ms: ms_since(state.origin_ms, raw.t_ms),
            tilt: raw.tilt,
            size: Some(PointSize { w: width, h: width }),
        }
    }
}

fn distance(a: [f32; 2], b: [f32; 2]) -> f32 {
    (b[0] - a[0]).hypot(b[1] - a[1])
}

/// Whole milliseconds from `origin` to `t`, clamped into the point's
/// `t_ms`. Float-to-int has no `From`/`TryFrom`; `as` saturates, which is
/// the clamp we want for a clock that ran backwards or a stroke held for
/// 49 days.
fn ms_since(origin: f64, t: f64) -> u32 {
    (t - origin).round() as u32 // ast-grep-ignore: no-as-cast
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn raw(x: f32, y: f32, force: f32, t_ms: f64) -> RawSample {
        RawSample {
            x,
            y,
            force,
            t_ms,
            tilt: None,
        }
    }

    /// A straight line of `n` samples `step` apart in space and `dt` ms in time.
    fn line(n: usize, step: f32, dt: f64, force: f32) -> Vec<RawSample> {
        (0..n)
            .map(|i| raw(i as f32 * step, 0.0, force, 1000.0 + i as f64 * dt))
            .collect()
    }

    fn run(tool: Tool, size: f32, samples: &[RawSample]) -> Vec<StrokePoint> {
        let mut m = BrushModeler::new(tool, size);
        for &s in samples {
            m.push(s);
        }
        m.finish()
    }

    fn widths(points: &[StrokePoint]) -> Vec<f32> {
        points.iter().map(|p| p.size.map_or(0.0, |s| s.w)).collect()
    }

    fn mean_second_difference(points: &[StrokePoint]) -> f32 {
        let ys: Vec<f32> = points.iter().map(|p| p.y).collect();
        let n = ys.len().saturating_sub(2);
        if n == 0 {
            return 0.0;
        }
        ys.windows(3)
            .map(|w| (w[2] - 2.0 * w[1] + w[0]).abs())
            .sum::<f32>()
            / n as f32
    }

    #[test]
    fn deterministic() {
        let samples = line(20, 2.0, 8.0, 0.6);
        assert_eq!(run(Tool::Pen, 4.0, &samples), run(Tool::Pen, 4.0, &samples));
    }

    #[test]
    fn streamline_reduces_jitter() {
        // A horizontal line with alternating ±1 vertical noise.
        let noisy: Vec<RawSample> = (0..40)
            .map(|i| {
                let jitter = if i % 2 == 0 { 1.0 } else { -1.0 };
                raw(i as f32 * 2.0, jitter, 0.5, 1000.0 + i as f64 * 8.0)
            })
            .collect();
        let raw_points: Vec<StrokePoint> = noisy
            .iter()
            .map(|r| StrokePoint {
                x: r.x,
                y: r.y,
                force: r.force,
                t_ms: 0,
                tilt: None,
                size: None,
            })
            .collect();
        let smoothed = run(Tool::Pen, 4.0, &noisy);
        assert!(smoothed.len() > 10, "smoothing dropped too many points");
        assert!(
            mean_second_difference(&smoothed) < mean_second_difference(&raw_points) / 2.0,
            "streamline should at least halve the jitter"
        );
    }

    #[test]
    fn pen_thins_with_speed() {
        let slow = run(Tool::Pen, 4.0, &line(30, 1.0, 16.0, 1.0));
        let fast = run(Tool::Pen, 4.0, &line(30, 6.0, 4.0, 1.0));
        // Compare mid-stroke, clear of both tapers and the speed EMA warm-up.
        let mid = |p: &[StrokePoint]| widths(p)[p.len() / 2];
        assert!(
            mid(&fast) < mid(&slow),
            "fast {} slow {}",
            mid(&fast),
            mid(&slow)
        );
        assert!(mid(&slow) <= 4.0 + 1e-5);
        assert!(mid(&fast) >= 1.0);
    }

    #[test]
    fn pen_thins_with_light_force() {
        let hard = run(Tool::Pen, 4.0, &line(30, 2.0, 8.0, 1.0));
        let soft = run(Tool::Pen, 4.0, &line(30, 2.0, 8.0, 0.3));
        let mid = |p: &[StrokePoint]| widths(p)[p.len() / 2];
        assert!(mid(&soft) < mid(&hard));
        assert!(mid(&soft) >= 1.0, "never below the width floor");
    }

    #[test]
    fn constant_width_tools_ignore_force_and_speed() {
        for tool in [Tool::Marker, Tool::Monoline, Tool::Brush] {
            let varied: Vec<RawSample> = (0..30)
                .map(|i| {
                    let force = if i % 3 == 0 { 0.2 } else { 1.0 };
                    let step = if i % 2 == 0 { 1.0 } else { 9.0 };
                    raw(i as f32 * step, 0.0, force, 1000.0 + i as f64 * 8.0)
                })
                .collect();
            let points = run(tool, 6.0, &varied);
            assert!(
                widths(&points).iter().all(|w| (w - 6.0).abs() < 1e-5),
                "{tool:?}: {:?}",
                widths(&points)
            );
        }
    }

    #[test]
    fn predict_never_mutates() {
        let mut m = BrushModeler::new(Tool::Pen, 4.0);
        for s in line(10, 2.0, 8.0, 0.7) {
            m.push(s);
        }
        let before = m.clone();
        let tail: Vec<RawSample> = (10..14)
            .map(|i| raw(i as f32 * 2.0, 1.0, 0.7, 1000.0 + i as f64 * 8.0))
            .collect();
        let predicted = m.predict(&tail);
        assert_eq!(m, before);
        assert!(!predicted.is_empty());

        // And prediction is exactly what pushing would have produced.
        let pushed: Vec<StrokePoint> = tail.iter().filter_map(|&s| m.push(s)).collect();
        assert_eq!(predicted, pushed);
    }

    #[test]
    fn finish_lands_on_last_raw_sample_and_tapers() {
        let samples = line(30, 2.0, 8.0, 0.8);
        let mut m = BrushModeler::new(Tool::Pen, 4.0);
        for &s in &samples {
            m.push(s);
        }
        let last_raw = samples[samples.len() - 1];
        let live = m.points().to_vec();
        let done = m.finish();

        // Streamline lags: the live tail is short of the pen.
        assert!(live[live.len() - 1].x < last_raw.x);
        let end = done[done.len() - 1];
        assert_eq!((end.x, end.y), (last_raw.x, last_raw.y));
        assert_eq!(end.t_ms, 29 * 8);

        let w = widths(&done);
        let mid = w[w.len() / 2];
        assert!(w[w.len() - 1] < mid, "tail should taper: {w:?}");
        assert!(w[w.len() - 1] >= 1.0, "taper stops at the floor: {w:?}");
        // Points before the taper zone are untouched.
        assert_eq!(w[..5], widths(&live)[..5]);
    }

    #[test]
    fn dot_keeps_its_width() {
        let done = run(Tool::Pen, 4.0, &[raw(10.0, 10.0, 1.0, 5.0)]);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].size, Some(PointSize { w: 4.0, h: 4.0 }));
        assert_eq!((done[0].x, done[0].y, done[0].t_ms), (10.0, 10.0, 0));
    }

    #[test]
    fn near_duplicates_are_dropped() {
        let mut m = BrushModeler::new(Tool::Pen, 4.0);
        assert!(m.push(raw(0.0, 0.0, 0.5, 0.0)).is_some());
        // 0.1 unit raw step, halved by streamline: below min_distance.
        assert!(m.push(raw(0.1, 0.0, 0.5, 4.0)).is_none());
        assert!(m.push(raw(5.0, 0.0, 0.5, 8.0)).is_some());
        assert_eq!(m.points().len(), 2);
    }

    #[test]
    fn timestamps_count_from_first_sample() {
        let points = run(Tool::Monoline, 2.0, &line(4, 3.0, 8.5, 1.0));
        let t: Vec<u32> = points.iter().map(|p| p.t_ms).collect();
        assert_eq!(t[0], 0);
        assert!(t.windows(2).all(|w| w[0] <= w[1]), "{t:?}");
        assert_eq!(t[t.len() - 1], 26);
    }

    #[test]
    fn tilt_passes_through() {
        let tilt = Tilt {
            azimuth: 1.0,
            altitude: 0.5,
            roll: 0.2,
        };
        let mut m = BrushModeler::new(Tool::Brush, 8.0);
        m.push(RawSample {
            tilt: Some(tilt),
            ..raw(0.0, 0.0, 1.0, 0.0)
        });
        assert_eq!(m.points()[0].tilt, Some(tilt));
    }

    fn arb_sample() -> impl Strategy<Value = RawSample> {
        (-1e4_f32..1e4, -1e4_f32..1e4, -0.5_f32..1.5, 0.0_f64..1e6)
            .prop_map(|(x, y, force, t_ms)| raw(x, y, force, t_ms))
    }

    proptest! {
        #[test]
        fn output_is_bounded_and_finite(
            tool in prop_oneof![Just(Tool::Pen), Just(Tool::Marker), Just(Tool::Monoline), Just(Tool::Brush)],
            size in 0.5_f32..40.0,
            samples in proptest::collection::vec(arb_sample(), 0..64),
        ) {
            let mut m = BrushModeler::new(tool, size);
            for &s in &samples {
                m.push(s);
            }
            let done = m.finish();
            prop_assert_eq!(done.is_empty(), samples.is_empty());
            let floor = size * m.params().min_width;
            for p in &done {
                prop_assert!(p.x.is_finite() && p.y.is_finite());
                prop_assert!((0.0..=1.0).contains(&p.force));
                let w = p.size.map_or(0.0, |s| s.w);
                prop_assert!(w >= floor - 1e-4 && w <= size + 1e-4, "width {w} outside [{floor}, {size}]");
            }
            if let (Some(last), Some(raw)) = (done.last(), samples.last()) {
                prop_assert_eq!((last.x, last.y), (raw.x, raw.y));
            }
        }
    }
}
