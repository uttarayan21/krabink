//! Brush model: turns raw pen samples into stroke points that carry their
//! rendered width, so every platform draws the same ink from the same
//! input. One [`BrushModeler`] lives per stroke in progress.
//!
//! Two stages. [`input`] smooths raw samples into the points a stroke
//! stores ([`EmaModel`]). [`dynamics`] folds those points into tip states,
//! the width at each point from force, speed and the tapers
//! ([`TipEvaluator`]). The modeler runs both and hands out points with
//! their sizes; [`BrushModeler::finish`] flushes the smoothing lag by
//! landing on the last raw sample exactly and applies the end taper.
//! [`BrushModeler::predict`] runs the same maths on a scratch copy for the
//! renderer's predicted-touch tail.
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

mod dynamics;
mod input;

pub use dynamics::{BrushParams, TipEvaluator, TipState};
pub use input::{EmaModel, InputParams, RawSample};

use crate::stroke::{PointSize, StrokePoint, Tool};

/// Turns one stroke's raw samples into modelled [`StrokePoint`]s. Owns the
/// points emitted so far; renderers draw [`points`](Self::points) plus a
/// [`predict`](Self::predict) tail every frame and commit
/// [`finish`](Self::finish) at pen-up.
#[derive(Debug, Clone, PartialEq)]
pub struct BrushModeler {
    tool: Tool,
    input: EmaModel,
    tip: TipEvaluator,
    /// Emitted points with their tip size applied.
    points: Vec<StrokePoint>,
    states: Vec<TipState>,
}

impl BrushModeler {
    /// A modeler for `tool` with the tool's tuned parameters at `size`.
    pub fn new(tool: Tool, size: f32) -> Self {
        Self::with_params(tool, BrushParams::for_tool(tool, size))
    }

    pub fn with_params(tool: Tool, params: BrushParams) -> Self {
        Self {
            tool,
            input: EmaModel::new(params.input()),
            tip: TipEvaluator::new(params),
            points: Vec::new(),
            states: Vec::new(),
        }
    }

    pub fn tool(&self) -> Tool {
        self.tool
    }

    pub fn params(&self) -> &BrushParams {
        self.tip.params()
    }

    /// Points emitted so far, without the end taper.
    pub fn points(&self) -> &[StrokePoint] {
        &self.points
    }

    /// Feed one raw sample; the point it produced, if it moved far enough
    /// from the previous one to be worth emitting.
    pub fn push(&mut self, raw: RawSample) -> Option<StrokePoint> {
        let point = self.input.push(raw)?;
        let state = self.tip.push(&point);
        let sized = sized(point, state);
        self.points.push(sized);
        self.states.push(state);
        Some(sized)
    }

    /// The points `raw` would produce if pushed now, without pushing them.
    /// For Apple's predicted touches: draw as a tail, discard next frame.
    pub fn predict(&self, raw: &[RawSample]) -> Vec<StrokePoint> {
        let mut tip = self.tip;
        self.input
            .predict(raw)
            .into_iter()
            .map(|p| sized(p, tip.push(&p)))
            .collect()
    }

    /// The finished stroke: every emitted point, the last raw sample
    /// landed exactly (streamline always lags the pen), and the end taper.
    pub fn finish(&self) -> Vec<StrokePoint> {
        let mut points = self.points.clone();
        let mut states = self.states.clone();
        if let Some(landing) = self.input.landing() {
            let mut tip = self.tip;
            let state = tip.push(&landing);
            points.push(sized(landing, state));
            states.push(state);
        }
        TipEvaluator::taper_end(self.params(), &mut states);
        points
            .into_iter()
            .zip(states)
            .map(|(p, s)| sized(p, s))
            .collect()
    }
}

fn sized(p: StrokePoint, s: TipState) -> StrokePoint {
    StrokePoint {
        size: Some(PointSize { w: s.w, h: s.h }),
        ..p
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stroke::Tilt;
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

    /// The widths a receiver computes from the stored points alone equal
    /// the widths the sender drew: the fold depends on nothing but the
    /// points.
    #[test]
    fn stored_points_reproduce_live_widths() {
        let mut m = BrushModeler::new(Tool::Pen, 4.0);
        for s in line(30, 2.0, 8.0, 0.8) {
            m.push(s);
        }
        let inputs: Vec<StrokePoint> = m
            .points()
            .iter()
            .map(|p| StrokePoint { size: None, ..*p })
            .collect();
        let refolded = TipEvaluator::evaluate(*m.params(), &inputs);
        let live = widths(m.points());
        let again: Vec<f32> = refolded.iter().map(|s| s.w).collect();
        assert_eq!(live, again);
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
